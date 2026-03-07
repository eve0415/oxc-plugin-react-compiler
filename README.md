# oxc-plugin-react-compiler

⚠️ **WARNING: Early Development Phase** ⚠️
This project is in its very early stages and is **NOT** ready for production use.

## About the Project
`oxc-plugin-react-compiler` is an experimental project aiming to port the original [`babel-plugin-react-compiler`](https://github.com/facebook/react/tree/main/compiler) to a Rust-based implementation on top of the [OXC](https://github.com/oxc-project/oxc) toolchain. 

The primary goal is to achieve **exact behavior** alignment with the original React Compiler implementation while leveraging the performance benefits of Rust. 

## Current Status
We have achieved a significant milestone in conformance testing: **1752 parity successes, 0 parity failures, 0 skipped**. This was verified by running the `conformance` test suite against the upstream React Compiler fixtures (`cargo run --package conformance --bin conformance -- --update --include-errors --verbose`). While still experimental, this demonstrates near-complete behavioral alignment with the original implementation.

## Experimental Nature
This project serves as an experiment to explore AI-assisted development in complex compiler porting tasks. The codebase is being heavily co-developed with AI assistants including Claude Code and Codex. Due to API rate limits and the inherent complexity of the task, development is expected to span several months. The initial primitive implementation took about a month to prove the viability of a Rust-based React compiler.

## Goals & Roadmap
- [x] Primitive implementation proving viability in Rust.
- [x] Align behavior exactly with the upstream React Compiler (verified via conformance tests).
- [ ] Provide seamless integration with **Vite v8** (currently in beta).
- [ ] Offer a Rolldown / Vite plugin.
- [ ] Future exploration: Add support for SWC alongside OXC.

## Architecture Overview
- **`crates/oxc_react_compiler/`**: Core compiler implementation (HIR, inference, reactive scopes, optimization, etc.).
- **`crates/oxc_react_compiler_napi/`**: N-API bindings to expose the Rust implementation to JavaScript.
- **`napi/`**: The publishable JavaScript wrapper.
- **`tasks/conformance/`**: Test harness ensuring parity with upstream React Compiler fixtures.

## Acknowledgements & Credits
This project wouldn't be possible without the incredible work of the following teams and tools:
- **[The React Team](https://react.dev/)**: For the original React Compiler architecture, logic, and conformance fixtures.
- **[The OXC Team](https://oxc.rs/)**: For the blazingly fast Rust-based JavaScript toolchain that powers this port.
- **[Claude Code](https://claude.ai/code)** and **[Codex](https://openai.com/blog/openai-codex)**: Heavily relied upon to analyze, explore, implement, test, and literally anything else to this project.

## Support the Project
If you find this project interesting or useful, please consider giving it a ⭐ on GitHub! Your support helps show that there is interest in a high-performance, Rust-based React Compiler.

## License
This project is open-source and will be licensed under the MIT License (or similar permissive open-source license, aligning with the ecosystem).
