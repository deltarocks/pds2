# Changelog

<!-- bureaucrate goes here -->

## [v0.1.2] 2026-11-24

### Added features

#### Types in module packages

A package written with `--module` may hold types, generated into its module
next to the definition output, which is what a `service` lowers to. Types
outside packages that use them move into the module too, and anything else
using a module package type fails to generate.

### Other features

- TypeScript codegen

### Dependencies

- Updated `pds2-core` to 0.2.0
- Updated `pds2-specializer-bytes` to 0.1.2
- Updated `pds2-specializer-protodef` to 0.1.2

## [v0.1.1] 2026-11-22

### Other changes

- refactor: Switch to 2024 edition

### Dependencies

- Updated `pds2-core` to 0.1.1
- Updated `pds2-specializer-bytes` to 0.1.1
- Updated `pds2-specializer-protodef` to 0.1.1
