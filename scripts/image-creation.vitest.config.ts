import { defineConfig } from "vitest/config";
export default defineConfig({
  test: {
    include: ["scripts/image-creation.acceptance.ts"],
    testTimeout: 30000,
  },
});
