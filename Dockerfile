# syntax=docker/dockerfile:1
# Static musl build into an empty image: about 20 MB, no shell, runs as a non-root user.
FROM rust:1-alpine AS build
RUN apk add --no-cache build-base
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
RUN cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/rift-recap /rift-recap
USER 65532:65532
ENTRYPOINT ["/rift-recap"]
# Mount a folder holding config.toml, players.json and .env at /bot (see docker-compose.yml).
CMD ["--config", "/bot/config.toml", "--env-file", "/bot/.env", "run"]
