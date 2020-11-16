# Kafka Schema Registry Tool

[![Crates.io](https://img.shields.io/crates/v/ksrt.svg)](https://crates.io/crates/ksrt/)
[![Docs.rs](https://docs.rs/ksrt/badge.svg)](https://docs.rs/ksrt/)

This command-line tool aims to make it easier for developers who utilize the [Confluent Schema Registry](https://github.com/confluentinc/schema-registry) to manage message schemas stored in it.

Primarily, it provides the ability to publish up-to-date message schemas to the registry, as well as retrieve current schemas from the registry.

In its initial release, it only supports [protobuf](https://github.com/protocolbuffers/protobuf) schemas.

## Installation

```sh
cargo install ksrt
```

## Usage

To see the supported commands and their options:

```sh
ksrt -h
```

## Examples

Post a protobuf schema and all its dependencies to the Schema Registry:

```sh
ksrt post -T protobuf -t access_log --strip-comments -f ~/protobuf/access_log.proto http://cp-schema-registry.local:8081
```

## License

Licensed under the [MIT license](LICENSE).
