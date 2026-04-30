FROM node:20-alpine AS builder
WORKDIR /app
COPY site/package.json site/package-lock.json ./site/
RUN cd site && npm ci
COPY docs/ ./docs/
COPY site/ ./site/
RUN cd site && npm run build

FROM nginxinc/nginx-unprivileged:1.27-alpine
COPY site/nginx-default.conf /etc/nginx/conf.d/default.conf
COPY --from=builder /app/site/build /usr/share/nginx/html
EXPOSE 8080
