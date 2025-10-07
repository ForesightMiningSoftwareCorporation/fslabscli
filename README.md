<div align="center">

# FSLABSCLI

## License

fslabscli is free and open source! All code in this repository is dual-licensed under either:

* MIT License ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

at your option. This means you can select the license you prefer! This dual-licensing approach is the de-facto standard in the Rust ecosystem and there are very good reasons to include both.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Installation

To install, run the following command:
``cargo install --git https://github.com/ForesightMiningSoftwareCorporation/fslabscli``

## Release Process
``` mermaid
sequenceDiagram
    participant Developer as Developer
    participant GitHub as GitHub
    participant Action as GitHub Action (Release Drafter)
    participant Webhook as Webhook to Prow
    participant Prow as Prow
    participant Release as GitHub Release

    loop Until New Bump of version in Cargo.toml
    Developer->>GitHub: Merge PR to main
    GitHub->>Action: Trigger GitHub Action
    Action->>Release: Create or update draft release with tag from Cargo.toml
    end

    loop Until Release mark as latest
        Developer->>Release: Publish
        Release->>Prow: Webhook to Prow with created tag
        Prow->>Prow: Build assets
        Prow->>Release: Upload assets to GitHub release
    end

    Developer->>Release: User marks the release as latest

```
