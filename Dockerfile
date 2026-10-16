# Ignis 多阶段构建：rust 构建镜像 → slim 运行镜像（只带二进制，内置技能已嵌入）。
#
# 冒烟：
#   docker build -t ignis .
#   docker run --rm -it -v ~/.ignis:/root/.ignis ignis doctor

FROM rust:1-slim AS build
# bundled SQLite（rusqlite）需要一个 C 编译器与 C 库头文件；slim 镜像不一定自带
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release -p ignis-cli

FROM debian:bookworm-slim
# TLS 根证书走 rustls 的 webpki-roots（编译进二进制），运行镜像无需 ca-certificates
COPY --from=build /src/target/release/ignis /usr/local/bin/ignis
VOLUME /root/.ignis
ENTRYPOINT ["ignis"]
