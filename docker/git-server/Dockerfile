FROM alpine:3.19

# git            — bare repos + core git tools
# git-daemon     — provides git-http-backend at /usr/libexec/git-core/git-http-backend
# lighttpd       — lightweight HTTP server with mod_cgi
# curl, jq       — used by pre-receive hook to call daemon auth API
RUN apk add --no-cache git git-daemon lighttpd curl jq

# The pre-receive hook template. Installed into each bare repo's hooks/ dir
# by the daemon (via `GitServerManager::create_repo`).
COPY pre-receive /usr/local/bin/git-pre-receive-hook

# lighttpd config: routes all HTTP to git-http-backend via mod_cgi.
COPY lighttpd.conf /etc/lighttpd/lighttpd.conf

# Entrypoint handles startup setup.
COPY entrypoint.sh /usr/local/bin/entrypoint.sh

RUN chmod +x /usr/local/bin/git-pre-receive-hook /usr/local/bin/entrypoint.sh \
    && mkdir -p /repos /var/www/localhost/htdocs /run/lighttpd \
    && chown lighttpd:lighttpd /run/lighttpd

# Verify git-http-backend exists at the expected path.
RUN test -x /usr/libexec/git-core/git-http-backend \
    || { echo "ERROR: git-http-backend not found"; exit 1; }

EXPOSE 80

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
