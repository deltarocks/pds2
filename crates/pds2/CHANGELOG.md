# Changelog

<!-- bureaucrate goes here -->

## [v0.2.0] 2026-11-24

### Breaking changes

- Packages + deduplication

### Added features

- Packages + deduplication

### Bugfixes

- Survey hashes ignore named argument order
- Merge hashes reference cycles as a whole

  A definition on a cycle kept an alias to an older version when another member of the cycle changed, because its hash was memoized while the cycle was still being walked.

### Dependencies

- Updated `pds2-cli` to 0.2.0
- Updated `pds2-core` to 0.2.0
- Updated `pds2-specializer-bifrostlink` to 0.1.2
- Updated `pds2-specializer-bytes` to 0.1.2
- Updated `pds2-specializer-csn1` to 0.1.2
- Updated `pds2-specializer-generic` to 0.1.2
- Updated `pds2-specializer-minecraft-data` to 0.2.0
- Updated `pds2-specializer-protobuf` to 0.1.2
- Updated `pds2-specializer-protodef` to 0.1.2

## [v0.1.1] 2026-11-22

### Other changes

- refactor: Switch to 2024 edition

### Dependencies

- Updated `pds2-cli` to 0.1.1
- Updated `pds2-core` to 0.1.1
- Updated `pds2-specializer-bifrostlink` to 0.1.1
- Updated `pds2-specializer-bytes` to 0.1.1
- Updated `pds2-specializer-csn1` to 0.1.1
- Updated `pds2-specializer-generic` to 0.1.1
- Updated `pds2-specializer-minecraft-data` to 0.1.1
- Updated `pds2-specializer-protobuf` to 0.1.1
- Updated `pds2-specializer-protodef` to 0.1.1
- Updated `protobuf-descriptor` to 0.1.1
