# The docs site image: nginx serving what `cargo xtask site` built, for local
# preview (`task docs-serve`) and docs.yml's build check. Nothing publishes it:
# the site is deployed to Netlify (docs-publish.yml).
#
# There is no build stage. The site is generated before `docker build`, by
#
#   cargo xtask site site/build/public --nginx-redirects site/build/nginx-redirects.conf
#
# run from the repository root (docs.yml does this in a job that holds no
# secret). Generating it outside the image keeps the Rust toolchain out of it
# and serves the very files the Netlify deploy publishes.
FROM nginxinc/nginx-unprivileged:1.31-alpine
COPY site/nginx-default.conf /etc/nginx/conf.d/default.conf
# Kept out of the served root: it is config, not content.
COPY site/build/nginx-redirects.conf /etc/nginx/site-redirects.conf
COPY site/build/public /usr/share/nginx/html
EXPOSE 8080
