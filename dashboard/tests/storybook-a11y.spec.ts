import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import type { Page as AxePage } from "playwright-core";
import { visualStoryCases } from "./storybook-cases";
import { prepareStory } from "./storybook-utils";

test.describe("dashboard story accessibility", () => {
  for (const story of visualStoryCases) {
    test(`${story.label} has no WCAG A/AA violations`, async ({ page }) => {
      await prepareStory(page, story, "dark", "comfortable");
      const results = await new AxeBuilder({ page: page as unknown as AxePage })
        .withTags(["wcag2a", "wcag2aa", "wcag21a", "wcag21aa"])
        .analyze();

      expect(results.violations).toEqual([]);
    });
  }
});
