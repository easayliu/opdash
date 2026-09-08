# ---------- 前端构建 ----------
FROM node:22-alpine AS ui
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ./
RUN npm install -g pnpm@10 && pnpm install --frozen-lockfile
# vite.config.ts 从 ../Cargo.toml 读版本号（读不到会退回 dev，不会挂），先拷进来让页脚显示对
COPY Cargo.toml /src/Cargo.toml
COPY ui ./
RUN pnpm build

# ---------- Rust 构建 ----------
FROM rust:1-slim AS builder
WORKDIR /src

# 先只用一个空壳 crate 把依赖编出来，单独占一层。只要 Cargo.toml / Cargo.lock 没动，
# 这层就能命中 buildcache，改 src 不用重编一遍所有依赖。
# 空壳阶段还没有 ui/dist：rust-embed 的宏只在我们自己的 crate 里展开，空 main 不会读它。
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
# rust-embed 在编译期读取 ui/dist（相对 crate 根）
COPY --from=ui /src/ui/dist ./ui/dist
# cargo 按 mtime 判断新旧，COPY 进来的文件可能比空壳产物还旧，
# 不 touch 一遍它会以为已经编好了，直接把空壳二进制交出去。
RUN find src -name '*.rs' -exec touch {} + \
    && cargo build --release --locked --bin opdash

# ---------- 运行时 ----------
FROM debian:stable-slim
# reqwest 用 rustls，只需要根证书（ClickHouse 走 https 时用）
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/opdash /usr/local/bin/opdash
EXPOSE 4880
# 所有配置都能用 OPDASH_* 环境变量给，见 README「配置」
ENTRYPOINT ["opdash"]
