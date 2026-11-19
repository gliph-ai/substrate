# Substrate cloud: one static Rust binary (axum + Postgres), migrations and
# static assets compiled in. The CLI is built and published separately
# (cli/release.sh); only the server ships in this image.
FROM rust:1.96-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY proto proto
COPY server server
COPY cli cli
RUN cargo build --release -p substrate-server --locked

FROM alpine:3.22
RUN apk add --no-cache ca-certificates && adduser -D -H -u 10001 substrate
COPY --from=build /src/target/release/substrate-server /usr/local/bin/substrate-server
USER substrate
ENV PORT=8080
EXPOSE 8080
CMD ["substrate-server"]
