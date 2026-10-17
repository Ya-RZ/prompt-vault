# prompt-vault CI 构建环境
#
# 为什么要自定义镜像：一条流水线里必须同时具备三样东西，官方镜像没有任何一个能全给。
#   1. Node 22 + pnpm     —— 前端构建（Vite 8 要求 Node ≥ 20.19 / 22.12）
#   2. Rust + gcc/g++/cmake —— 本项目有 C 与 C++ 依赖：rusqlite(bundled) 编 SQLite，qdrant-edge 带 cpp/
#   3. mingw-w64          —— Windows 交叉编译（x86_64-pc-windows-gnu）
#
# 另外镜像里预置了 windows-gnu target，省掉每次流水线都 rustup target add。
#
# ⚠️ 镜像里**不含**任何模型权重，也不含 Python —— 编码侧车是独立进程，
#    按开发文档 §10 铁律 8，模型绝不进任何二进制或镜像。

FROM node:22-bookworm-slim

ARG RUST_VERSION=1.99.0
ARG PNPM_VERSION=12.8.1
ARG DEBIAN_FRONTEND=noninteractive

# build-essential: rusqlite bundled 要 cc；g++/cmake: qdrant-edge 的 cpp 部分；
# gcc-mingw-w64/g++-mingw-w64: Windows 交叉编译；pkg-config/xz-utils: 常见构建依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config cmake \
        ca-certificates curl git xz-utils jq \
        gcc-mingw-w64-x86-64 g++-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/*

# Rust 工具链。若要加速可加上国内镜像（二选一，取消注释即可）：
#   ENV RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static \
#       RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain ${RUST_VERSION} \
    && rustup target add x86_64-pc-windows-gnu \
    # minimal profile 不含 clippy/rustfmt，而 push 流水线只用 clippy 做门禁
    && rustup component add clippy rustfmt \
    && chmod -R a+rwX ${RUSTUP_HOME} ${CARGO_HOME}

# pnpm 版本与仓库 lockfile 对齐（package.json 没写 packageManager 字段，所以显式钉住）
RUN npm install -g pnpm@${PNPM_VERSION} \
    && pnpm --version \
    && node --version \
    && rustc --version \
    && x86_64-w64-mingw32-gcc --version | head -1
