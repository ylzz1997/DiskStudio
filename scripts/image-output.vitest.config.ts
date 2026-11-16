import { defineConfig } from "vitest/config";
// Explicit ordinary-file integration acceptance, excluded from the default
// *.test.ts suite. The corpus must be produced by the Rust example in .vm.
export default defineConfig({
  test: { include: ["scripts/image-output.acceptance.ts"], testTimeout: 30000 },
});
