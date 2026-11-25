# Windows quick start

The `x86_64-pc-windows-msvc` package is intended for 64-bit Windows 10 and later. Clean-machine compatibility has not yet been verified, so this intended minimum is not a guarantee for every Windows installation.

1. Download and extract `hecate-v0.1.0-x86_64-pc-windows-msvc.zip`.
2. Run `hecate.exe --help`, or launch `hecate.exe` in an interactive terminal to open the menu.

The archive's `hecate.exe` is built with static MSVC CRT linkage. It still uses Windows system APIs, and this build has not been verified on a clean Windows machine. HECATE does not bundle Git; install Git and make it available on `PATH` for workflows that invoke Git, including workspace preparation and contribution workflows.

GitHub discovery uses network access. A GitHub token is optional. Provider configuration is needed only for `contribute --generate`; `contribute --proposal FILE` uses a supplied proposal and does not require model-provider configuration. See [README.md](README.md) and [PROVIDERS.md](docs/PROVIDERS.md) for command and configuration details.

The ZIP contains the MIT and Apache-2.0 license texts. HECATE is dual-licensed under either license, at your option.
