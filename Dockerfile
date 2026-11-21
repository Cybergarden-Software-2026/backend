# syntax=docker/dockerfile:1
#
# Backend дашборда. Контекст сборки — корень репозитория (что в него входит — .dockerignore).
#
#   docker build -t dashboard-backend .
#   docker run --rm --env-file .env dashboard-backend /usr/local/bin/migrate
#   docker run --rm --env-file .env -p 8080:8080 dashboard-backend
#
# Обычно запускается через docker-compose.yml.
#
# Сборщик и runtime должны быть на одном выпуске Debian (glibc): trixie ↔ debian13.
ARG RUST_IMAGE=rust:1-slim-trixie
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot

# ---------- тесты: PostgreSQL подключается при запуске контейнера ----------
FROM ${RUST_IMAGE} AS test
WORKDIR /src
COPY . ./
RUN cargo test --workspace --locked --no-run
CMD ["cargo", "test", "--workspace", "--locked", "--offline"]

# ---------- cargo-chef: кэш зависимостей отдельным слоем ----------
FROM ${RUST_IMAGE} AS chef
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS planner
COPY . ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
# SQL миграций встраивается в бинарник (include_str! в crates/store).
COPY . ./
RUN cargo build --release --locked --bin api --bin migrate --bin importer --bin ai-worker

# ---------- runtime ----------
FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=builder /src/target/release/api /src/target/release/migrate \
    /src/target/release/importer /src/target/release/ai-worker /usr/local/bin/
WORKDIR /app
USER nonroot
ENV API_HOST=0.0.0.0 API_PORT=8080
EXPOSE 8080
# В distroless нет curl: `api --healthcheck` сам делает GET /health.
HEALTHCHECK --interval=10s --timeout=3s --retries=5 CMD ["/usr/local/bin/api", "--healthcheck"]
ENTRYPOINT []
CMD ["/usr/local/bin/api"]
