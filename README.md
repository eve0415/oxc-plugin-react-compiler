# oxc-plugin-react-compiler

[![npm version](https://img.shields.io/npm/v/oxc-plugin-react-compiler)](https://www.npmjs.com/package/oxc-plugin-react-compiler)
[![license](https://img.shields.io/npm/l/oxc-plugin-react-compiler)](LICENSE)

> [!WARNING]
> **Experimental**: This project may have edge cases where behavior differs from the official React Compiler plugin. If you encounter any differences, please [open an issue](https://github.com/eve0415/oxc-plugin-react-compiler/issues/new) with a detailed description and a minimal reproduction so we can investigate.

## About the Project

`oxc-plugin-react-compiler` is an experimental project aiming to port the original [`babel-plugin-react-compiler`](https://github.com/facebook/react/tree/main/compiler) to a Rust-based implementation on top of the [OXC](https://github.com/oxc-project/oxc) toolchain.

> [!NOTE]
> **Disclaimer:** This is a community-driven project. It is **not** an official project from the React team or OXC, and we are not affiliated with Meta, the React core team, or the OXC project.

The primary goal is to achieve **exact behavior** alignment with the original React Compiler implementation while leveraging the performance benefits of Rust.

## Current Status

We have achieved **100% conformance parity** with the upstream React Compiler. This was verified by running the full conformance test suite against all upstream fixtures. While still experimental, this demonstrates complete behavioral alignment with the original implementation.

## Installation

```bash
npm install oxc-plugin-react-compiler
```

> [!NOTE]
> This plugin requires **Vite 8.0 or later**.

## Usage

```ts
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { reactCompilerOxc } from 'oxc-plugin-react-compiler';

export default defineConfig({
  plugins: [
    reactCompilerOxc({
      // compilationMode: 'infer',  // 'infer' | 'annotation' | 'all'
      // panicThreshold: 'none',    // 'none' | 'all'
      // target: '19',              // React version to target
      // sources: ['src/'],         // string[] | ((id: string) => boolean)
    }),
    react(),
  ],
});
```

## Experimental Nature

This project serves as an experiment to explore AI-assisted development in complex compiler porting tasks. The codebase is being heavily co-developed with AI assistants including Claude Code and Codex. Due to API rate limits and the inherent complexity of the task, development is expected to span several months. The initial primitive implementation took about a month to prove the viability of a Rust-based React compiler.

## Goals & Roadmap

- [x] Primitive implementation proving viability in Rust.
- [x] Align behavior exactly with the upstream React Compiler (verified via conformance tests).
- [x] Provide seamless integration with **Vite v8** via the included Vite plugin.
- [x] Publish to npm for easy installation.
- [x] Source map support.
- [ ] Future exploration: Add support for SWC alongside OXC.

## Architecture Overview

- **`crates/oxc_react_compiler/`**: Core compiler implementation (HIR, inference, reactive scopes, optimization, codegen).
- **`crates/oxc_react_compiler_napi/`**: N-API bindings to expose the Rust implementation to JavaScript.
- **`napi/`**: JavaScript wrapper + Vite v8 plugin.
- **`tasks/conformance/`**: Test harness ensuring parity with upstream React Compiler fixtures.

## Acknowledgements & Credits

This project wouldn't be possible without the incredible work of the following teams and tools:

- **[The React Team](https://react.dev/)**: For the original React Compiler architecture, logic, and conformance fixtures.
- **[The OXC Team](https://oxc.rs/)**: For the blazingly fast Rust-based JavaScript toolchain that powers this port.
- **[Claude Code](https://claude.ai/code)** and **[Codex](https://openai.com/blog/openai-codex)**: Heavily relied upon to analyze, explore, implement, test, and literally anything else to this project.

## Support the Project

If you find this project interesting or useful, please consider giving it a ⭐ on GitHub! Your support helps show that there is interest in a high-performance, Rust-based React Compiler.

Found a bug or unexpected behavior? Please [open an issue](https://github.com/eve0415/oxc-plugin-react-compiler/issues/new) — detailed reports help us improve.

## License

This project is open-source and licensed under the [MIT License](LICENSE).
