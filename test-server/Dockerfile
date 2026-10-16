# gsvn test server: Apache httpd + mod_dav_svn + mod_ssl.
# Fedora 44 ships Subversion 1.14.5, matching the repos made on the host.
FROM fedora:44

RUN dnf -y install --setopt=install_weak_deps=False \
        httpd mod_dav_svn mod_ssl httpd-tools openssl \
    && dnf clean all

# Basic-auth user test/test, realm "gsvn test".
RUN htpasswd -bcB /etc/httpd/gsvn.htpasswd test test \
    && chmod 0644 /etc/httpd/gsvn.htpasswd

# Self-signed certificate (CN/SAN = localhost, 127.0.0.1) so that clients get
# "issuer is not trusted" (unknown-ca) but no hostname mismatch.
RUN mkdir -p /etc/httpd/gsvn-tls \
    && openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/O=gsvn test/CN=localhost" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
        -addext "basicConstraints=critical,CA:FALSE" \
        -addext "extendedKeyUsage=serverAuth" \
        -keyout /etc/httpd/gsvn-tls/server.key \
        -out /etc/httpd/gsvn-tls/server.crt \
    && chmod 0644 /etc/httpd/gsvn-tls/server.key /etc/httpd/gsvn-tls/server.crt

COPY httpd.conf /etc/httpd/gsvn-httpd.conf

# The container is meant to run as the host user's uid (docker run --user) so
# the bind-mounted repositories are readable without changing their ownership.
# Everything httpd needs to write goes to /tmp.
EXPOSE 8080 8443
CMD ["/usr/sbin/httpd", "-f", "/etc/httpd/gsvn-httpd.conf", "-DFOREGROUND"]
