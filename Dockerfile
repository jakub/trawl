FROM debian:bookworm-slim

ARG TARGETARCH

LABEL org.opencontainers.image.title="trawl" \
      org.opencontainers.image.description="Self-hosted log collection, storage, and search" \
      org.opencontainers.image.url="https://github.com/jakub/trawl" \
      org.opencontainers.image.source="https://github.com/jakub/trawl" \
      org.opencontainers.image.licenses="MPL-2.0"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libstdc++6 libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

COPY docker-ctx/${TARGETARCH}/bin/trawld docker-ctx/${TARGETARCH}/bin/trawl-admin docker-ctx/${TARGETARCH}/bin/fleet-admin docker-ctx/${TARGETARCH}/bin/trawl-web /usr/bin/
COPY docker-ctx/${TARGETARCH}/lib/trawl/ /usr/lib/trawl/
COPY docker-ctx/${TARGETARCH}/LICENSE docker-ctx/${TARGETARCH}/LICENSE.duckdb docker-ctx/${TARGETARCH}/distribution.json /usr/share/doc/trawl/

# trawld-file-capability: cap_sys_ptrace=p
RUN apt-get update \
    && apt-get install -y --no-install-recommends libcap2-bin \
    && setcap cap_sys_ptrace+p /usr/bin/trawld \
    && [ "$(getcap /usr/bin/trawld)" = "/usr/bin/trawld cap_sys_ptrace=p" ] \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd -r trawl \
    && useradd -r -g trawl -s /usr/sbin/nologin -d /var/lib/trawl trawl \
    && mkdir -p /var/lib/trawl /etc/trawl \
    && chown trawl:trawl /var/lib/trawl

USER trawl

EXPOSE 5514 1514/udp 1514/tcp 8090

ENTRYPOINT ["trawld"]
CMD ["--config", "/etc/trawl/trawld.toml", "--no-monitor"]
