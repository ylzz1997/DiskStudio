import { defineConfig } from "vitest/config";
export default defineConfig({
  test: { include: ["scripts/image-browse.acceptance.ts"], testTimeout: 30000 },
});
