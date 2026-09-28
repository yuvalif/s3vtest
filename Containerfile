# Build:  podman build -t s3vtest .
# Run:    podman run --rm --network host -v "$PWD":/work:Z s3vtest run job.yaml

# ---- build stage ----
FROM docker.io/library/rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends libhdf5-dev pkg-config cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# build the dependencies first so that they are cached across source changes
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo build --release --locked --features hdf5 \
 && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked --features hdf5

# ---- runtime stage ----
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libhdf5-103-1 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/s3vtest /usr/local/bin/s3vtest
COPY examples /usr/share/s3vtest/examples
# configuration files, reports and the dataset cache live in the mounted directory
WORKDIR /work
ENTRYPOINT ["s3vtest"]
CMD ["--help"]
