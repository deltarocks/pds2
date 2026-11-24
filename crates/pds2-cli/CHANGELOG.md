# Changelog

<!-- bureaucrate goes here -->

## [v0.2.0] 2026-11-24

### Breaking changes

- Packages + deduplication

### Added features

#### Types in module packages

A package written with `--module` may hold types, generated into its module
next to the definition output, which is what a `service` lowers to. Types
outside packages that use them move into the module too, and anything else
using a module package type fails to generate.

### Other features

- Packages + deduplication

### Dependencies

- Updated `pds2-core` to 0.2.0

## [v0.1.1] 2026-11-22

### Other changes

- refactor: Switch to 2024 edition

### Dependencies

- Updated `pds2-core` to 0.1.1
