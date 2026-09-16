import path from "node:path";
import tailwindcss from "@tailwindcss/vite";
import type { StorybookConfig } from "@storybook/react-vite";
import { mergeConfig } from "vite";

const config: StorybookConfig = {
  stories: ["../src/**/*.stories.@(ts|tsx)"],
  addons: ["@storybook/addon-a11y"],
  framework: {
    name: "@storybook/react-vite",
    options: {}
  },
  async viteFinal(baseConfig) {
    return mergeConfig(baseConfig, {
      plugins: [tailwindcss()],
      resolve: {
        alias: {
          "@": path.resolve(__dirname, "../src")
        }
      },
      server: {
        allowedHosts: ["localhost", "127.0.0.1"]
      }
    });
  }
};

export default config;
