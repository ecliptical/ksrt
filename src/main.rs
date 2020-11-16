use gumdrop::Options;
use log::*;
use prost::Message;
use prost_build::{
    protoc,
    protoc_include,
};

use prost_types::{
    FileDescriptorProto,
    FileDescriptorSet,
};

use regex::{
    Regex,
    RegexBuilder,
};

use schema_registry_converter::{
    blocking::schema_registry::{
        get_schema_by_subject,
        post_schema,
        SrSettings,
    },
    schema_registry_common::{
        get_subject,
        RegisteredReference,
        RegisteredSchema,
        SchemaType,
        SubjectNameStrategy,
        SuppliedReference,
        SuppliedSchema,
    },
};

use std::{
    collections::HashMap,
    fmt,
    fs,
    path::{
        Path,
        PathBuf,
    },
    process::Command,
    str::FromStr,
};

use tracing_subscriber::{
    fmt::Subscriber as TracingSubscriber,
    EnvFilter as TracingEnvFilter,
};

#[allow(dead_code)]
mod built_info;

/// Manage schemas in the Kafka Schema Registry.
#[derive(Debug, Options)]
struct Settings {
    /// print usage and exit
    help: bool,

    /// command
    #[options(command, required)]
    command: Option<Cmd>,
}

#[derive(Debug, Options)]
enum Cmd {
    /// retrieve an existing schema
    Get(GetSettings),

    /// post a schema to the Kafka Schema Registry
    Post(PostSettings),
}

/// Retrieve an existing schema from the Kafka Schema Registry.
#[derive(Debug, Options)]
struct GetSettings {
    /// print usage and exit
    help: bool,

    /// topic name (required unless `--record' is specified)
    #[options(meta = "NAME")]
    topic: Option<String>,

    /// whether the schema is for the topic key (vs. value)
    #[options(short = "k")]
    topic_key: bool,

    /// record name (required unless `--topic' is specified)
    #[options(meta = "NAME")]
    record: Option<String>,

    /// Schema Registry URL(s) (required)
    #[options(free, required)]
    schema_registry_url: Vec<String>,
}

/// Post a schema to the Kafka Schema Registry.
/// This will create a new schema version for the given subject *unless*
/// there is already an existing version with the equivalent schema.
#[derive(Debug, Options)]
struct PostSettings {
    /// print usage and exit
    help: bool,

    /// schema type (required; one of `avro', `json', or `protobuf')
    #[options(long = "type", meta = "TYPE", required, short = "T")]
    schema_type: SchemaTypeOpt,

    /// topic name (required unless `--record' is specified)
    #[options(meta = "NAME")]
    topic: Option<String>,

    /// whether the schema is for the topic key (vs. value)
    #[options(short = "k")]
    topic_key: bool,

    /// record name (required unless `--topic' is specified)
    #[options(meta = "NAME")]
    record: Option<String>,

    /// schema file (required)
    #[options(required)]
    file: PathBuf,

    /// include directory for any references (optional; could be multiple)
    #[options(meta = "DIR")]
    include: Vec<PathBuf>,

    /// strip comments
    #[options(no_short)]
    strip_comments: bool,

    /// Schema Registry URL(s) (required)
    #[options(free, required)]
    schema_registry_url: Vec<String>,
}

#[derive(Debug)]
#[non_exhaustive]
enum SchemaTypeOpt {
    Avro,
    Json,
    Protobuf,
}

impl Default for SchemaTypeOpt {
    fn default() -> Self {
        Self::Avro
    }
}

impl fmt::Display for SchemaTypeOpt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Avro => write!(f, "avro"),
            Self::Json => write!(f, "json"),
            Self::Protobuf => write!(f, "protobuf"),
        }
    }
}

impl FromStr for SchemaTypeOpt {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let schema_type = match s {
            "avro" => Self::Avro,
            "json" => Self::Json,
            "protobuf" => Self::Protobuf,
            _ => anyhow::bail!("unsupported schema type"),
        };

        Ok(schema_type)
    }
}

impl From<SchemaTypeOpt> for SchemaType {
    fn from(src: SchemaTypeOpt) -> Self {
        match src {
            SchemaTypeOpt::Avro => Self::Avro,
            SchemaTypeOpt::Json => Self::Json,
            SchemaTypeOpt::Protobuf => Self::Protobuf,
        }
    }
}

fn parse_protos<P>(protos: &[P], includes: &[P]) -> anyhow::Result<FileDescriptorSet>
where
    P: AsRef<Path>,
{
    let tmp = tempfile::Builder::new().prefix("prost-build").tempdir()?;
    let descriptor_set = tmp.path().join("prost-descriptor-set");

    let mut cmd = Command::new(protoc());
    cmd.arg("--include_imports")
        .arg("--include_source_info")
        .arg("-o")
        .arg(&descriptor_set);

    for include in includes {
        cmd.arg("-I").arg(include.as_ref());
    }

    // Set the protoc include after the user includes in case the user wants to
    // override one of the built-in .protos.
    cmd.arg("-I").arg(protoc_include());

    for proto in protos {
        cmd.arg(proto.as_ref());
    }

    let output = cmd.output()?;
    if !output.status.success() {
        return Err(anyhow::format_err!(
            "protoc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let buf = fs::read(descriptor_set)?;
    let descriptor_set = FileDescriptorSet::decode(&*buf)?;

    Ok(descriptor_set)
}

fn get_protobuf_references(
    fd: &FileDescriptorProto,
    fds: &[FileDescriptorProto],
    schemas: &HashMap<String, String>,
) -> anyhow::Result<Vec<SuppliedReference>> {
    fd.dependency
        .iter()
        .try_fold(Vec::with_capacity(fd.dependency.len()), |mut refs, name| {
            let fd = fds
                .iter()
                .find(|&dep| dep.name.as_deref() == Some(name))
                .ok_or_else(|| {
                    anyhow::format_err!("failed to locate file for dependency: {}", name)
                })?;

            let mt = fd.message_type.first().ok_or_else(|| {
                anyhow::format_err!("failed to locate a top-level message type in: {}", name)
            })?;

            let subject: Vec<_> = fd.package.iter().cloned().chain(mt.name.clone()).collect();
            let subject = subject.join(".");
            let schema = schemas
                .get(name)
                .ok_or_else(|| anyhow::format_err!("failed to locate schema for: {}", name))?;

            let sup_ref = SuppliedReference {
                name: name.clone(),
                subject,
                schema: schema.clone(),
                references: get_protobuf_references(fd, fds, schemas)?,
            };

            refs.push(sup_ref);
            Ok(refs)
        })
}

fn strip_comments(schema: String, ml_comment: &Regex, sl_comment: &Regex) -> String {
    let mut buf = None;
    let mut ml_start = 0;

    for ml_match in ml_comment.find_iter(&schema) {
        let lines = &schema[ml_start..ml_match.start()];
        ml_start = ml_match.end();

        let buf = buf.get_or_insert_with(|| String::with_capacity(schema.len()));

        let mut sl_start = 0;
        for sl_match in sl_comment.find_iter(lines) {
            buf.push_str(&lines[sl_start..sl_match.start()]);
            sl_start = sl_match.end();
        }

        buf.push_str(&lines[sl_start..]);
    }

    let lines = &schema[ml_start..];
    let mut sl_start = 0;
    for sl_match in sl_comment.find_iter(lines) {
        buf.get_or_insert_with(|| String::with_capacity(lines.len()))
            .push_str(&lines[sl_start..sl_match.start()]);
        sl_start = sl_match.end();
    }

    if let Some(mut buf) = buf {
        buf.push_str(&lines[sl_start..]);
        buf
    } else {
        schema
    }
}

fn post_avro_schema(_settings: &PostSettings) -> anyhow::Result<SuppliedSchema> {
    unimplemented!("avro schema not yet supported")
}

fn post_json_schema(_settings: &PostSettings) -> anyhow::Result<SuppliedSchema> {
    unimplemented!("json schema not yet supported")
}

fn post_protobuf_schema(settings: &PostSettings) -> anyhow::Result<SuppliedSchema> {
    let file = settings.file.canonicalize()?;
    let mut includes = Vec::with_capacity(settings.include.len() + 1);

    if let Some(dir) = file.parent() {
        includes.push(dir.canonicalize()?);
    }

    let mut includes = settings
        .include
        .iter()
        .try_fold(includes, |mut includes, path| {
            let path = path.canonicalize()?;
            includes.push(path);
            Ok::<_, anyhow::Error>(includes)
        })?;

    let mut fd_set = parse_protos(&[file.clone()], &includes)?;

    trace!("fd set: {:#?}", fd_set);

    includes.push(protoc_include().canonicalize()?);

    let ml_comment = RegexBuilder::new(r"/\*.*?\*/")
        .dot_matches_new_line(true)
        .build()?;
    let sl_comment = RegexBuilder::new(r"//.*$").multi_line(true).build()?;

    let schemas = fd_set.file.iter().try_fold(
        HashMap::with_capacity(fd_set.file.len()),
        |mut schemas, fd| {
            let name = fd
                .name
                .clone()
                .ok_or_else(|| anyhow::Error::msg("missing name in file descriptor"))?;

            let path = includes
                .iter()
                .find(|&path| path.join(&name).is_file())
                .ok_or_else(|| anyhow::format_err!("failed to locate file for: {}", name))?;

            let mut schema = fs::read_to_string(path.join(&name))?;

            if settings.strip_comments {
                // As of now, the Schema Registry doesn't exclude comments when comparing versions!
                schema = strip_comments(schema, &ml_comment, &sl_comment);
            }

            schemas.insert(name, schema);
            Ok::<_, anyhow::Error>(schemas)
        },
    )?;

    trace!("schemas: {:#?}", schemas);

    let root_fd = fd_set
        .file
        .pop()
        .expect("at least one file descriptor in the set");

    assert_eq!(
        file.file_name()
            .expect("supplied filename refers to a regular file")
            .to_string_lossy(),
        root_fd.name.as_deref().expect("file descriptor has a name")
    );

    let schema = SuppliedSchema {
        name: None,
        schema_type: SchemaType::Protobuf,
        schema: fs::read_to_string(file)?,
        references: get_protobuf_references(&root_fd, &fd_set.file, &schemas)?,
    };

    Ok(schema)
}

fn print_reference(reference: RegisteredReference) {
    println!("\tname: {}", reference.name);
    println!("\tsubject: {}", reference.subject);
    println!("\tversion: {}", reference.version);
}

fn print_schema(schema: RegisteredSchema) {
    println!("id: {}", schema.id);
    match schema.schema_type {
        SchemaType::Avro => println!("type: avro"),
        SchemaType::Json => println!("type: json"),
        SchemaType::Protobuf => println!("type: protobuf"),
        SchemaType::Other(value) => println!("type: {}", value),
    }

    println!("schema:");
    for line in schema.schema.lines() {
        println!("\t{}", line);
    }

    if !schema.references.is_empty() {
        println!("references:");
        for reference in schema.references {
            print_reference(reference);
        }
    }
}

fn run_get(sr_settings: SrSettings, subject: SubjectNameStrategy) -> anyhow::Result<()> {
    let reg = get_schema_by_subject(&sr_settings, &subject)
        .map_err(|e| anyhow::format_err!("error retrieving schema: {}", e))?;

    debug!("registered schema: {:#?}", reg);

    print_schema(reg);

    Ok(())
}

fn run_post(
    sr_settings: SrSettings,
    subject: String,
    schema: SuppliedSchema,
) -> anyhow::Result<()> {
    let reg = post_schema(&sr_settings, subject, schema)
        .map_err(|e| anyhow::format_err!("error posting schema: {}", e))?;

    debug!("registered schema: {:#?}", reg);

    print_schema(reg);

    Ok(())
}

fn schema_registry_settings_from_settings(
    urls: impl IntoIterator<Item = String>,
) -> anyhow::Result<SrSettings> {
    let mut urls = urls.into_iter();
    let mut builder = SrSettings::new_builder(urls.next().expect("at least one item"));
    urls.for_each(|url| {
        builder.add_url(url);
    });

    builder
        .build()
        .map_err(|e| anyhow::format_err!("error configuring schema registry client: {:?}", e))
}

fn subject_name_strategy_from_settings(
    topic: Option<String>,
    record: Option<String>,
    topic_key: bool,
) -> anyhow::Result<SubjectNameStrategy> {
    let sns = if let Some(topic) = topic {
        if let Some(record) = record {
            SubjectNameStrategy::TopicRecordNameStrategy(topic, record)
        } else {
            SubjectNameStrategy::TopicNameStrategy(topic, topic_key)
        }
    } else if let Some(record) = record {
        SubjectNameStrategy::RecordNameStrategy(record)
    } else {
        anyhow::bail!("either `--topic' or `--record' are required");
    };

    Ok(sns)
}

fn version() -> String {
    format!(
        "{} {} ({}, {} build, {} [{}], {})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        built_info::GIT_VERSION.unwrap_or("unknown"),
        built_info::PROFILE,
        built_info::CFG_OS,
        built_info::CFG_TARGET_ARCH,
        built_info::BUILT_TIME_UTC,
    )
}

fn main() -> anyhow::Result<()> {
    TracingSubscriber::builder()
        .with_env_filter(TracingEnvFilter::from_default_env())
        .init();

    info!("{}", version());

    let settings: Settings = Options::parse_args_default_or_exit();

    debug!("args: {:#?}", settings);

    let cmd = settings.command.expect("command");
    match cmd {
        Cmd::Get(settings) => {
            let sr_settings = schema_registry_settings_from_settings(settings.schema_registry_url)?;

            let sns = subject_name_strategy_from_settings(
                settings.topic,
                settings.record,
                settings.topic_key,
            )?;

            run_get(sr_settings, sns)
        }

        Cmd::Post(settings) => {
            let schema = match settings.schema_type {
                SchemaTypeOpt::Avro => post_avro_schema(&settings)?,
                SchemaTypeOpt::Json => post_json_schema(&settings)?,
                SchemaTypeOpt::Protobuf => post_protobuf_schema(&settings)?,
            };

            let sr_settings = schema_registry_settings_from_settings(settings.schema_registry_url)?;

            let sns = subject_name_strategy_from_settings(
                settings.topic,
                settings.record,
                settings.topic_key,
            )?;

            let subject = get_subject(&sns)
                .map_err(|e| anyhow::format_err!("error determining subject: {:?}", e))?;

            run_post(sr_settings, subject, schema)
        }
    }
}
