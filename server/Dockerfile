# The server binary is built ahead of time (musl static, x86_64) — the image
# only assembles the runtime so the 1-core production host never compiles.
FROM alpine:3.20

RUN apk add --no-cache ca-certificates git curl tini

COPY skills-manager-server /usr/local/bin/skills-manager-server
COPY public /app/public
RUN chmod +x /usr/local/bin/skills-manager-server

ENV SM_HOME=/data \
    SM_PORT=8080 \
    SM_STATIC_DIR=/app/public

VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["/sbin/tini", "--"]
CMD ["skills-manager-server"]
