FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd -r -u 10001 app && mkdir /data && chown app /data
COPY --from=build /src/target/release/papertrade /usr/local/bin/papertrade
COPY web /app/web
ENV WEB_DIR=/app/web DB_PATH=/data/papertrade.db BIND=0.0.0.0:8080
USER app
VOLUME /data
EXPOSE 8080
CMD ["papertrade"]
