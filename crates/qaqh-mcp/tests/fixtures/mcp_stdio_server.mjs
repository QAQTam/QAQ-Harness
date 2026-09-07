// M1-2 lifecycle 子进程 fixture：最小 stdio MCP server（newline-delimited JSON-RPC）。
//
// 模式（env `QAQH_TEST_MODE`）：
// - 默认：完整 legacy 握手（initialize / tools/list；未知方法回 -32601，
//   与 rmcp Auto 探测的 legacy 回退路径对齐——见 tests/lifecycle.rs 头注）。
// - "deaf"：接受 stdin 但从不回应（connect 超时用例；组杀验证的靶子）。
// - "envdump"：正常握手；pid 文件写三行——pid / API_KEY 回显 / MODE 回显
//   （M1-3 e2e：证明 ${secret:name} 在子进程收到的是解析值而非占位符）。
// - "grandchild"：正常握手 + 自行 spawn 一个 node 孙进程（继承同一进程组，
//   不 setsid）；pid 文件写两行——本进程 pid / 孙进程 pid（orphan_reap
//   验证 killpg 组杀覆盖 npx→node→server 式三层树）。
//
// 所有模式都会写 pid 文件（env `QAQH_TEST_PID_FILE`，测试据此轮询
// /proc/<pid> 验证 RAII reap），并在 stdin 关闭时退出。
import * as fs from "node:fs";
import * as readline from "node:readline";
import * as cp from "node:child_process";

const pidFile = process.env.QAQH_TEST_PID_FILE;
const mode = process.env.QAQH_TEST_MODE ?? "";
if (pidFile) {
  const pid = String(process.pid);
  let payload = pid;
  if (mode === "envdump") {
    payload = [pid, process.env.API_KEY ?? "<unset>", process.env.QAQH_TEST_MODE ?? "<unset>"].join("\n");
  } else if (mode === "grandchild") {
    const grandchild = cp.spawn(process.execPath, ["-e", "setInterval(() => {}, 1000);"], {
      stdio: "ignore",
    });
    payload = [pid, String(grandchild.pid)].join("\n");
  }
  fs.writeFileSync(pidFile, payload);
}

const deaf = mode === "deaf";
const slowMs = Number(process.env.QAQH_TEST_SLOW_MS ?? "0");

const send = (message) => {
  process.stdout.write(JSON.stringify(message) + "\n");
};

const rl = readline.createInterface({ input: process.stdin, terminal: false });

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed || deaf) {
    return;
  }
  let message;
  try {
    message = JSON.parse(trimmed);
  } catch {
    return;
  }
  if (message.method === "initialize") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        protocolVersion: message.params?.protocolVersion ?? "2025-11-25",
        capabilities: { tools: {} },
        serverInfo: { name: "qaqh-mcp-fixture", version: "0.1.0" },
      },
    });
  } else if (message.method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: {
        tools: [
          {
            name: "echo",
            description: "echo fixture tool",
            inputSchema: {
              type: "object",
              properties: { text: { type: "string", description: "text to echo back" } },
            },
          },
          {
            name: "slow",
            description: "sleeps QAQH_TEST_SLOW_MS then succeeds",
            inputSchema: { type: "object", properties: {} },
          },
        ],
      },
    });
  } else if (message.method === "tools/call") {
    const tool = message.params?.name ?? "";
    if (tool === "echo") {
      const text = message.params?.arguments?.text ?? "(no text)";
      send({
        jsonrpc: "2.0",
        id: message.id,
        result: { content: [{ type: "text", text: `echo: ${text}` }] },
      });
    } else if (tool === "slow") {
      // 慢工具：sleep 后回结果；测试在途中 SIGKILL 本进程即得真实 crash
      // （transport EOF → client TransportClosed）。
      setTimeout(() => {
        send({
          jsonrpc: "2.0",
          id: message.id,
          result: { content: [{ type: "text", text: "slow done" }] },
        });
      }, slowMs);
    } else {
      send({
        jsonrpc: "2.0",
        id: message.id,
        error: { code: -32602, message: `unknown tool ${tool}` },
      });
    }
  } else if (message.id !== undefined && message.id !== null) {
    send({
      jsonrpc: "2.0",
      id: message.id,
      error: { code: -32601, message: "method not found" },
    });
  }
  // notification（无 id）：不回应。
});

rl.on("close", () => process.exit(0));
