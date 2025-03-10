---
title: Production Checklist
permalink: /deployment/production-checklist
category: Deployment
menuOrder: 2
---

This is a checklist for configuring and securing Cube.js for a production
deployment.

## Enable Production Mode

When running Cube.js in production make sure `NODE_ENV` is set to `production` and `CUBEJS_DEV_MODE` is not set to `true`.
Some platforms, such as Heroku, do it by default.

In this mode, the insecure development server and Playground will be disabled
by default because there's a security risk serving those in production
environments. Production Cube.js servers can only be accessed through the
[REST API](rest-api) and Cube.js frontend libraries.

## Set up Redis

Cube.js requires [Redis](https://redis.io/), an in-memory data structure store,
to run in production.

It uses Redis for query caching and queue. Set the `REDIS_URL` environment
variable to allow Cube.js to connect to Redis. If your Redis instance also has
a password, please set it via the `REDIS_PASSWORD` environment variable. Set
the `REDIS_TLS` environment variable to `true` if you want to enable
SSL-secured connections. Ensure your Redis cluster allows at least 15
concurrent connections.

[[warning | Note]]
| Cube.js server instances used by same tenant environments should have same
| Redis instances. Otherwise they will have different query queues which can
| lead to incorrect pre-aggregation states and intermittent data access errors.

### Redis Pool

If `REDIS_URL` is provided Cube.js, will create a Redis connection pool with a
minimum of 2 and maximum of 1000 concurrent connections, by default.
The `CUBEJS_REDIS_POOL_MIN` and `CUBEJS_REDIS_POOL_MAX` environment variables
can be used to tweak pool size limits. To disable connection pooling, and
instead create connections on-demand, you can set `CUBEJS_REDIS_POOL_MAX` to 0.

If your maximum concurrent connections limit is too low, you may see
`TimeoutError: ResourceRequest timed out` errors. As a rule of a thumb, you
need to have `Queue Size * Number of tenants` concurrent connections to ensure
the best performance possible. If you use clustered deployments, please make
sure you have enough connections for all Cube.js server instances. A lower
number of connections still can work, however Redis becomes a performance
bottleneck in this case.

### Running without Redis

If you want to run Cube.js in production without Redis, you can use
`CUBEJS_CACHE_AND_QUEUE_DRIVER` environment variable to `memory`.

[[warning | Note]]
| Serverless and clustered deployments can't be run without Redis as it is used
| to manage the query queue.

## Set up Pre-aggregations Storage

If you are using [external pre-aggregations][link-pre-aggregations], you need
to set up and configure external pre-aggregations storage.

[link-pre-aggregations]: /pre-aggregations#external-pre-aggregations

Currently, we recommend using MySQL for external pre-aggregations storage.
There is some additional MySQL configuration required to optimize for
pre-aggregation ingestion and serving. The final configuration may vary
depending on the specific use case.

## Set up Refresh Worker

If you are using [scheduled pre-aggregations][link-scheduled-refresh], we
recommend running a separate Cube.js worker instance to refresh scheduled
pre-aggregations in the background. This allows your main Cube.js instance
to continue to serve requests with high availability.

[link-scheduled-refresh]: /pre-aggregations#scheduled-refresh

```bash
# Set to true so a Cube.js instance acts as a refresh worker
CUBEJS_SCHEDULED_REFRESH_TIMER=true
```

## Enable HTTPS

Production APIs should be served over HTTPS to be secure over the network.

Cube.js doesn't handle SSL/TLS for your API. To serve your API on HTTPS URL you
should use a reverse proxy, like [NGINX][link-nginx], [Kong][link-kong],
[Caddy][link-caddy] or your cloud provider's load balancer SSL termination
features.

[link-nginx]: https://www.nginx.com/
[link-kong]: https://konghq.com/kong/
[link-caddy]: https://caddyserver.com/

### NGINX Sample Configuration

Below you can find a sample `nginx.conf` to proxy requests to Cube.js. To learn
how to set up SSL with NGINX please refer to [NGINX docs][link-nginx-docs].

[link-nginx-docs]: https://nginx.org/en/docs/http/configuring_https_servers.html

```nginx
server {
  listen 80;
  server_name cube.my-domain.com;

  location / {
    proxy_pass http://localhost:4000/;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
  }
}
```

