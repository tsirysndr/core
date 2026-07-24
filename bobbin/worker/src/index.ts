import { Container } from "@cloudflare/containers";

export interface Env {
  BOBBIN: DurableObjectNamespace<BobbinContainer>;
  RATE_LIMITER: RateLimit;
  BOBBIN_HYDRANT_URL: string;
  BOBBIN_SLINGSHOT_URL: string;
  BOBBIN_LOG: string;
}

export class BobbinContainer extends Container {
  defaultPort = 8090;
  enableInternet = true;
  // Bobbin maintains an in-memory index rebuilt from Hydrant replay on every
  // restart, so we want to avoid sleeping where possible. Override
  // onActivityExpired() to keep the container alive indefinitely.
  sleepAfter = "24h";

  async onActivityExpired(): Promise<void> {
    // Keep the container running; bobbin's in-memory index is expensive to
    // rebuild. Renew the timeout instead of stopping so we're pinged again later.
    this.renewActivityTimeout();
  }

  onError(error: Error) {
    console.error("bobbin container error:", error);
  }
}

const INDEX = `This is bobbin, Tangled's stateless XRPC API service: https://tangled.org/tangled.org/core/tree/master/bobbin`;

const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, HEAD, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, Authorization",
  "Access-Control-Max-Age": "86400",
};

function withCors(response: Response): Response {
  const headers = new Headers(response.headers);
  for (const [name, value] of Object.entries(CORS_HEADERS)) {
    headers.set(name, value);
  }

  // Responses with these statuses must not carry a body; a non-null body
  // crashes workerd, so null it out.
  const body = [101, 204, 205, 304].includes(response.status)
    ? null
    : response.body;

  return new Response(body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: CORS_HEADERS });
    }

    const url = new URL(request.url);
    if (url.pathname === "/" || url.pathname === "") {
      return withCors(
        new Response(INDEX, { headers: { "Content-Type": "text/plain" } }),
      );
    }

    const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
    const { success } = await env.RATE_LIMITER.limit({ key: ip });
    if (!success) {
      return withCors(
        new Response(
          JSON.stringify({
            error: "RateLimitExceeded",
            message: "too many requests, slow down",
          }),
          {
            status: 429,
            headers: {
              "Content-Type": "application/json",
              "Retry-After": "60",
            },
          },
        ),
      );
    }

    const container = env.BOBBIN.getByName("primary");
    await container.startAndWaitForPorts({
      startOptions: {
        envVars: {
          BOBBIN_HYDRANT_URL: env.BOBBIN_HYDRANT_URL,
          BOBBIN_SLINGSHOT_URL: env.BOBBIN_SLINGSHOT_URL,
          BOBBIN_LOG: env.BOBBIN_LOG,
          BOBBIN_LOG_FORMAT: "json",
        },
      },
    });
    const response = await container.fetch(request);
    // A 101 is a protocol switch (e.g. WebSocket upgrade); return it untouched
    // so we don't strip the connection off the response.
    if (response.status === 101) {
      return response;
    }
    return withCors(response);
  },
} satisfies ExportedHandler<Env>;
