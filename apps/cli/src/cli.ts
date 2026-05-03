#!/usr/bin/env bun
import { Command } from "commander";
import { API_VERSION } from "@p2p-one/shared";

const program = new Command();

program
  .name("p2p-one")
  .description("P2P One CLI")
  .version("0.1.0");

program
  .command("status")
  .description("Check system status")
  .action(async () => {
    console.log(`P2P One CLI ${API_VERSION}`);
    console.log("Status: operational");
  });

program
  .command("api <endpoint>")
  .description("Call an internal API endpoint")
  .option("-m, --method <method>", "HTTP method", "GET")
  .option("-b, --body <body>", "Request body JSON")
  .action(async (endpoint, options) => {
    const baseUrl = process.env.P2P_API_URL || "http://localhost:3000";
    const url = `${baseUrl}/api/${endpoint}`;

    const res = await fetch(url, {
      method: options.method,
      headers: { "Content-Type": "application/json" },
      body: options.body ? JSON.stringify(JSON.parse(options.body)) : undefined,
    });

    const data = await res.json();
    console.log(JSON.stringify(data, null, 2));
  });

program.parse();
