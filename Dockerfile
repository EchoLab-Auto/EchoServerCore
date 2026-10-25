# esc-gateway 容器镜像
#
# ⚠ 部署时**必须**用 `--network host`（或 compose 的 network_mode: host）：
#   网关要访问宿主机 127.0.0.1 上的面板/服务。容器默认的 bridge 网络里
#   `127.0.0.1` 指的是容器自己，转发会连不上。详见 docs/GATEWAY.md。
#
# 构建：
#   docker build -t esc-gateway:latest .
#   国内网络若拉不动 crates.io / github，见文末的 CARGO_MIRROR / GIT_PROXY。

# ─────────────────────────────────────────────────────────────────────────────
# 阶段 1：构建
# ─────────────────────────────────────────────────────────────────────────────
# 必须 >= workspace 的 rust-version（1.98），否则构建直接失败。
FROM rust:1.99-slim-bookworm AS builder

# 构建期需要的工具。git 是必须的：workspace 里有 git 依赖（plugin-framework），
# cargo 解析整个 workspace 时要能 clone 到它们——即使本 crate 不用。
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# ── 可选：国内镜像 ────────────────────────────────────────────────────────
# 构建参数，默认留空（走官方源）。国内构建示例：
#   docker build \
#     --build-arg CARGO_MIRROR=sparse+https://rsproxy.cn/index/ \
#     --build-arg GIT_PROXY=https://gh-proxy.com/https://github.com/ \
#     -t esc-gateway .
ARG CARGO_MIRROR=""
ARG GIT_PROXY=""
RUN mkdir -p /root/.cargo \
 && { \
      echo '[net]'; echo 'retry = 10'; echo 'git-fetch-with-cli = true'; \
      if [ -n "$CARGO_MIRROR" ]; then \
        echo '[source.crates-io]'; echo "replace-with = \"mirror\""; \
        echo '[source.mirror]'; echo "registry = \"$CARGO_MIRROR\""; \
      fi; \
    } > /root/.cargo/config.toml \
 && if [ -n "$GIT_PROXY" ]; then \
      git config --global url."$GIT_PROXY".insteadOf "https://github.com/"; \
    fi

# 先只拷依赖清单，利用层缓存：改业务代码时不必重新拉依赖。
COPY Cargo.toml Cargo.lock ./
COPY crates/esc-common/Cargo.toml       crates/esc-common/
COPY crates/esc-control-api/Cargo.toml  crates/esc-control-api/
COPY crates/esc-engine/Cargo.toml       crates/esc-engine/
COPY crates/esc-hub/Cargo.toml          crates/esc-hub/
COPY crates/esc-spoke/Cargo.toml        crates/esc-spoke/
COPY crates/esc-cli/Cargo.toml          crates/esc-cli/
COPY crates/esc-host/Cargo.toml         crates/esc-host/
COPY crates/esc-panel-auth/Cargo.toml   crates/esc-panel-auth/
COPY crates/esc-plugin-tunnel/Cargo.toml        crates/esc-plugin-tunnel/
COPY crates/esc-plugin-tunnel-panel/Cargo.toml  crates/esc-plugin-tunnel-panel/
COPY crates/esc-server-panel/Cargo.toml crates/esc-server-panel/
COPY crates/esc-gateway/Cargo.toml      crates/esc-gateway/

# 给每个 crate 造一个空的 lib/main，让 `cargo fetch` 能解析出完整依赖图。
# 这一步只为把「拉依赖」缓存成独立层——它不产出可用的二进制。
RUN for d in crates/*/; do \
      mkdir -p "$d/src"; \
      [ -f "$d/src/lib.rs" ] || echo "" > "$d/src/lib.rs"; \
    done \
 && for b in esc-cli esc-host esc-gateway esc-server-panel esc-plugin-tunnel-panel; do \
      mkdir -p "crates/$b/src"; \
      [ -f "crates/$b/src/main.rs" ] || echo "fn main(){}" > "crates/$b/src/main.rs"; \
    done \
 && cargo fetch --locked

# 再拷真正的源码并构建。只要 esc-gateway 的产物。
COPY crates ./crates
RUN cargo build --release --locked --bin esc-gateway \
 && strip target/release/esc-gateway

# ─────────────────────────────────────────────────────────────────────────────
# 阶段 2：运行
# ─────────────────────────────────────────────────────────────────────────────
# 用 debian-slim 而不是 distroless cc-debian12。
#
# 原因是实测：gcr.io 在国内直连不通（i/o timeout），daocloud 的 gcr 代理返回
# 403，ghcr 也 denied。而 docker.io 的 debian 走 registry-mirrors 能拉。
# 「镜像更小」不值得换来一个拉不下来的 Dockerfile。
#
# 代价：镜像大了约 20 MB，且多了 /bin/sh。为此做两件事补偿：
#   - 非 root 运行（uid 65532，与 distroless 的 nonroot 同号）
#   - compose 里 read_only + cap_drop ALL + no-new-privileges
# 想要 distroless 的可以自己把下面一行换掉（前提是那台机器拉得到）。
FROM debian:bookworm-slim

COPY --from=builder /src/target/release/esc-gateway /usr/local/bin/esc-gateway

# 默认读这个路径；用 volume 挂进来
ENV ESC_GATEWAY_CONFIG=/etc/esc/gateway.toml

# 非 root 运行。65532 是 distroless 的 nonroot uid，保持一致便于迁移。
# 用数字而不是用户名：这个镜像里没有对应条目，写名字会起不来。
USER 65532:65532

# 注意：**不 EXPOSE**。用 host 网络时端口由配置决定，
# EXPOSE 只会误导人以为要 -p 映射。
#
# 健康检查：用网关自己的 `health` 子命令，它会真的向第一个 route 的端口发
# /__auth/healthz 并检查 200。
#
# 不用 `curl`/`CMD-SHELL` 写管道：健康检查在**目标镜像**里执行，而镜像里
# 不该为了探活多装一个 curl（那会连带拉进 10+ MB 依赖）。用一个自己写的
# 子命令，无论目标是 distroless 还是 slim 都能跑。
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/esc-gateway", "health", "-c", "/etc/esc/gateway.toml"]

ENTRYPOINT ["/usr/local/bin/esc-gateway"]
CMD ["-c", "/etc/esc/gateway.toml"]
