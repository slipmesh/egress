# Changelog

All notable changes to this project will be documented in this file.

This project adheres to [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
and follows [Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-11-28

### Added ✨

- Report each source's last answer in the status (#17)

### Build system 🛠️

- Update sliplib to v0.28.2 (#16)

## [0.1.0] - 2026-11-27

### Breaking changes 💥

- Remove via from the Egress resource (#7)

### Added ✨

- Define the Egress resource and print its CRD (#2)
- Print the Egress resources of a version 1 file's bypass lists (#3)
- Resolve a list's sources to networks of both families (#4)
- Resolve each Egress that names the node and report its status (#9)
- Install each list's routes on the nodes it names (#11)
- Leave every node's endpoint out of each list (#13)

### Build system 🛠️

- Update sliplib to v0.28.1 (#12)
- Update non-major (#5)

### CI/CD ⚙️

- Keep renovate off the version 1 pins of egress-export (#8)
- Attach the deploy files to the release (#14)

### Changed 🔧

- Remove egress-export (#10)

### Miscellaneous 🧹

- Start the repository
- Lay out the repository (#1)
