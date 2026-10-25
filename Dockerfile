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
# distroless：没有 shell、没有包管理器，攻击面最小。
# 换 base 镜像时要注意：它必须带 CA 证书目录（本程序目前不主动出网，但留着无害）。
FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /src/target/release/esc-gateway /usr/local/bin/esc-gateway

# 默认读这个路径；用 volume 挂进来
ENV ESC_GATEWAY_CONFIG=/etc/esc/gateway.toml

# 非 root 运行。distroless 的 nonroot 是 uid 65532。
USER nonroot:nonroot

# 注意：**不 EXPOSE**。用 host 网络时端口由配置决定，
# EXPOSE 只会误导人以为要 -p 映射。
#
# 健康检查：**不能用 CMD-SHELL**（distroless 里没有 /bin/sh），也不能用
# `--version`（只证明进程活着，不证明它还在接受连接）。所以用网关自己的
# `health` 子命令：它会真的向第一个 route 的端口发 /__auth/healthz 并检查 200。
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/esc-gateway", "health", "-c", "/etc/esc/gateway.toml"]

ENTRYPOINT ["/usr/local/bin/esc-gateway"]
CMD ["-c", "/etc/esc/gateway.toml"]
