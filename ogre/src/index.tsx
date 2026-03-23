import { cardPayloadSchema } from "./validation";
import { renderCard } from "./lib/render";
import { RepositoryCard } from "./components/cards/repository";
import { IssueCard } from "./components/cards/issue";
import { PullRequestCard } from "./components/cards/pull-request";
import { z } from "zod";

declare global {
  interface CacheStorage {
    default: Cache;
  }
}

interface Env {
  ENVIRONMENT: string;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method !== "POST") {
      return new Response("Method not allowed", { status: 405 });
    }

    const url = new URL(request.url);
    const cardType = url.pathname.split("/").pop();

    try {
      const body = await request.json();
      const payload = cardPayloadSchema.parse(body);

      let component;
      switch (payload.type) {
        case "repository":
          component = <RepositoryCard {...payload} />;
          break;
        case "issue":
          component = <IssueCard {...payload} />;
          break;
        case "pullRequest":
          component = <PullRequestCard {...payload} />;
          break;
        default:
          return new Response("Unknown card type", { status: 400 });
      }

      const cacheKeyUrl = new URL(request.url);
      cacheKeyUrl.searchParams.set("payload", JSON.stringify(payload));
      const cacheKey = new Request(cacheKeyUrl.toString(), { method: "GET" });
      const cache = caches.default;
      const cached = await cache.match(cacheKey);

      if (cached) {
        return cached;
      }

      const { png: pngBuffer } = await renderCard(component);

      const response = new Response(pngBuffer as any, {
        headers: {
          "Content-Type": "image/png",
          "Cache-Control": "public, max-age=3600",
        },
      });

      await cache.put(cacheKey, response.clone());

      return response;
    } catch (error) {
      if (error instanceof z.ZodError) {
        return new Response(
          JSON.stringify({ errors: (error as z.ZodError).issues }),
          {
            status: 400,
            headers: { "Content-Type": "application/json" },
          },
        );
      }

      console.error("Error generating card:", error);
      const errorMessage =
        error instanceof Error ? error.message : String(error);
      const errorStack = error instanceof Error ? error.stack : "";
      console.error("Error stack:", errorStack);
      return new Response(
        JSON.stringify({
          error: errorMessage,
          stack: errorStack,
        }),
        {
          status: 500,
          headers: { "Content-Type": "application/json" },
        },
      );
    }
  },
};
