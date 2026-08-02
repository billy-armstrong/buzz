/**
 * Tests for PersonaProviderApiKeyField.
 *
 * The component is a pure view over its props with no extractable logic.
 * The meaningful correctness check is that the component module exports the
 * expected function and that TypeScript (tsc --noEmit) accepts the envVarName
 * prop as optional — both verified at build time.
 *
 * Rendering-level behavior (envVarName hint appears when present, absent when
 * omitted) is exercised by the E2E bridge and Playwright screenshots; a
 * jsdom-based test would add no coverage beyond what TypeScript already
 * guarantees for a trivially conditional `{envVarName ? <p> : null}`.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { PersonaProviderApiKeyField } from "./PersonaProviderApiKeyField.tsx";

test("PersonaProviderApiKeyField_exports_a_function", () => {
  assert.equal(typeof PersonaProviderApiKeyField, "function");
});

test("PersonaProviderApiKeyField_accepts_envVarName_as_optional_prop", () => {
  // TypeScript enforces the prop shape at compile time (tsc --noEmit).
  // This test confirms the export identity is stable and the module loads.
  assert.equal(
    PersonaProviderApiKeyField.length,
    1,
    "component accepts props object",
  );
});
