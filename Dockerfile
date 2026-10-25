FROM node:24.18.0-bookworm-slim@sha256:6f7b03f7c2c8e2e784dcf9295400527b9b1270fd37b7e9a7285cf83b6951452d AS web
WORKDIR /src
COPY package*.json ./
RUN npm ci --ignore-scripts
COPY web ./web
COPY shared ./shared
COPY tsconfig.json vite.config.ts ./
RUN npm run build:web

FROM rust:1.99.1-slim-bookworm@sha256:86234e857a413004348b471d11a4ebe1aa92d2eae2b6b143cac68fd1ac3c48be AS native
ARG TARGETARCH=amd64
ARG PROFILE=full
ARG SOURCE_REVISION=unversioned
ARG SOURCE_DATE_EPOCH=0
ENV KVMHELM_BUILD_REVISION=$SOURCE_REVISION SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH
WORKDIR /src
# Build tools stay in this stage. The final image contains no compiler/npm or
# reference TypeScript daemon. Runtime base/helper versions are pinned below.
RUN test "$TARGETARCH" = amd64 \
    && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/20261024T000000Z bookworm main\ndeb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/20261024T000000Z bookworm-security main\n' > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get update && apt-get install -y --no-install-recommends cmake make g++ perl pkg-config libatomic1 && rm -rf /var/lib/apt/lists/*
COPY --from=web /usr/local/bin/node /usr/local/bin/node
COPY . .
COPY --from=web /src/dist/web ./dist/web
RUN node scripts/package-native.mjs --target x86_64-unknown-linux-gnu --profile "$PROFILE" --out /package \
    && mkdir /image \
    && tar -xzf "$(find /package -name '*.tar.gz' -type f)" --strip-components=1 -C /image

FROM debian:bookworm-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8
ARG PROFILE=full
LABEL org.kvmhelm.runtime="rust" org.kvmhelm.package.profile="$PROFILE"
WORKDIR /app
ENV KVMHELM_DATA=/data KVMHELM_SECRETS=/secrets HOME=/home/kvmhelm
# C/C++ runtime dependencies come from the pinned build image; no
# floating runtime apt installation is performed in this final image.
COPY --from=native /usr/lib/x86_64-linux-gnu/libstdc++.so.6* /usr/lib/x86_64-linux-gnu/
COPY --from=native /usr/lib/x86_64-linux-gnu/libatomic.so.1* /usr/lib/x86_64-linux-gnu/
COPY --from=native /lib/x86_64-linux-gnu/libgcc_s.so.1 /lib/x86_64-linux-gnu/
COPY --from=native /usr/share/doc/libstdc++6/copyright /app/licenses/system/libstdc++6-copyright
COPY --from=native /usr/share/doc/libatomic1/copyright /app/licenses/system/libatomic1-copyright
COPY --from=native /usr/share/doc/libgcc-s1/copyright /app/licenses/system/libgcc-s1-copyright
COPY --from=native /usr/share/common-licenses/ /app/licenses/system/common/
COPY --from=native --chown=1000:1000 /image/ /app/
RUN groupadd --gid 1000 kvmhelm && useradd --uid 1000 --gid 1000 --create-home --shell /usr/sbin/nologin kvmhelm
USER 1000:1000
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s CMD ["/app/kvmhelm", "healthcheck"]
ENTRYPOINT ["/app/kvmhelm"]
CMD ["serve"]
