# The docs site image: nginx serving what `cargo xtask site` built.
#
# There is no build stage. The site is generated before `docker build`, by
#
#   cargo xtask site site/build/public --nginx-redirects site/build/nginx-redirects.conf
#
# run from the repository root (docs.yml and docs-publish.yml do this in a job
# that holds no secret). Generating it outside the image keeps the Rust
# toolchain out of it and lets the Netlify deploy publish the very same files.
FROM nginxinc/nginx-unprivileged:1.31-alpine
COPY site/nginx-default.conf /etc/nginx/conf.d/default.conf
# Kept out of the served root: it is config, not content.
COPY site/build/nginx-redirects.conf /etc/nginx/site-redirects.conf
COPY site/build/public /usr/share/nginx/html
EXPOSE 8080
