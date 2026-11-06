# syntax=docker/dockerfile:1.7
#
# Asmoday 容器镜像：Vite 前端 → Rust musl 静态二进制 → 极简运行层。
#
#   docker build -t asmoday .
#   docker run -d --name asmoday -p 4242:4242 -p 6881:6881 -p 6881:6881/udp \
#     -v asmoday-data:/data asmoday
#
# 首次构建要在 musl 上重编整棵依赖树（ring / rusqlite-bundled / librqbit），
# 十几分钟是正常的；后面靠 BuildKit 的 cargo 缓存层复用。
#
# 运行层不装 openssl：全项目走 rustls（见 Cargo.toml 的平台分流注释），
# 这也是这里能编出静态二进制、镜像能小到 ~30 MB 的原因。

# ---------- 前端 ----------
FROM node:22-alpine AS web
WORKDIR /app/web
RUN corepack enable
# 先只拷清单：依赖层不随源码改动失效
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web/ ./
# vite 的 outDir 是 ../dist
RUN pnpm run build

# ---------- 后端 ----------
FROM rust:alpine AS build
# musl 目标上 ring / rusqlite(bundled) 需要 C 工具链；openssl 一律不装
RUN apk add --no-cache build-base musl-dev perl cmake
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY vendor ./vendor
COPY build.rs ./
COPY src ./src
COPY --from=web /app/dist ./dist
# 镜像里没有 .git（进上下文只会让构建变慢），版本信息由 build-arg 注入；
# 不给就用 build.rs 的 dev 回退
ARG BUILD_ID=""
ENV ASMODAY_BUILD_ID=${BUILD_ID}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/asmoday /usr/local/bin/asmoday

# ---------- 运行 ----------
FROM alpine:3.21
RUN apk add --no-cache ca-certificates tzdata \
 && adduser -D -u 10001 -h /data asmoday
COPY --from=build /usr/local/bin/asmoday /usr/local/bin/asmoday

USER asmoday
# config.toml、asmoday.db、缩略图缓存、下载文件都落在 /data
WORKDIR /data
VOLUME ["/data"]

# 4242：Web UI / API；6881：BT 的 TCP + UDP（DHT/Peer）
EXPOSE 4242
EXPOSE 6881/tcp
EXPOSE 6881/udp

# auto：容器里先试 :: 双栈，Docker 默认 bridge 没有 IPv6 时自动降到 0.0.0.0。
# 写死 "::" 会在这种网络里直接起不来；写死 0.0.0.0 又拿不到双栈宿主机的 IPv6。
ENV ASMODAY_HOST=auto \
    ASMODAY_PORT=4242

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD wget -qO- http://127.0.0.1:4242/api/health >/dev/null || exit 1

ENTRYPOINT ["asmoday"]
CMD ["-d", "/data"]
