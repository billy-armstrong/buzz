/**
 * Behavioral tests for PersonaProviderApiKeyField.
 *
 * Tests the rendering invariants that matter for the disambiguation story:
 * - semantic label is present in the rendered output
 * - envVarName hint is rendered when the prop is present
 * - hint id is wired to the input via aria-describedby
 * - hint is absent when envVarName is omitted
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import { PersonaProviderApiKeyField } from "./PersonaProviderApiKeyField.tsx";

function makeProps(overrides = {}) {
  return {
    disabled: false,
    isInherited: false,
    inheritedLabel: "Set in global defaults",
    isRequired: false,
    label: "OpenAI Runtime API Key",
    onValueChange: () => {},
    value: "",
    ...overrides,
  };
}

test("PersonaProviderApiKeyField_renders_semantic_label", () => {
  const html = renderToStaticMarkup(
    React.createElement(PersonaProviderApiKeyField, makeProps()),
  );
  assert.ok(
    html.includes("OpenAI Runtime API Key"),
    "semantic label must appear in rendered output",
  );
});

test("PersonaProviderApiKeyField_renders_env_var_hint_when_envVarName_present", () => {
  const html = renderToStaticMarkup(
    React.createElement(
      PersonaProviderApiKeyField,
      makeProps({ envVarName: "OPENAI_COMPAT_API_KEY" }),
    ),
  );
  assert.ok(
    html.includes("OPENAI_COMPAT_API_KEY"),
    "env-var hint must appear when envVarName is provided",
  );
});

test("PersonaProviderApiKeyField_wires_hint_id_via_aria_describedby", () => {
  const html = renderToStaticMarkup(
    React.createElement(
      PersonaProviderApiKeyField,
      makeProps({ envVarName: "OPENAI_COMPAT_API_KEY" }),
    ),
  );
  // Both the hint element and the input must reference the same id.
  assert.ok(
    html.includes('id="persona-provider-api-key-hint"'),
    "hint paragraph must have the expected id",
  );
  assert.ok(
    html.includes('aria-describedby="persona-provider-api-key-hint"'),
    "input must reference the hint via aria-describedby",
  );
});

test("PersonaProviderApiKeyField_omits_hint_when_envVarName_absent", () => {
  const html = renderToStaticMarkup(
    React.createElement(PersonaProviderApiKeyField, makeProps()),
  );
  assert.ok(
    !html.includes("aria-describedby"),
    "no aria-describedby when envVarName is omitted",
  );
  assert.ok(
    !html.includes("persona-provider-api-key-hint"),
    "hint id must not appear when envVarName is omitted",
  );
});
