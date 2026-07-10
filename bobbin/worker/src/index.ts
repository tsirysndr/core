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

  onActivityExpired(): boolean {
    // Keep the container running; bobbin's in-memory index is expensive to rebuild.
    return true;
  }

  onError(error: Error) {
    console.error("bobbin container error:", error);
  }
}

const INDEX = `This is bobbin, Tangled's stateless XRPC API service: https://tangled.org/tangled.org/core/tree/master/bobbin`;

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/" || url.pathname === "") {
      return new Response(INDEX, { headers: { "Content-Type": "text/plain" } });
    }

    const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
    const { success } = await env.RATE_LIMITER.limit({ key: ip });
    if (!success) {
      return new Response(
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
    return container.fetch(request);
  },
} satisfies ExportedHandler<Env>;
