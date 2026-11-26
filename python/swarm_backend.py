# language: Python 3.10+, file: python/swarm_backend.py, runtime: Windows 11, target: Kimi CLI worker fan-out
"""Python execution backend for the temporary KimiChatCLI swarm MCP tool.

The Rust controller owns the HTTP MCP lifecycle.  This module owns the part
that benefits from the original implementation: a bounded Python worker pool
that launches independent Kimi CLI sessions and tears down each process tree
on timeout.  The controller sends one JSON request on stdin and receives one
JSON response on stdout, so the module has no network listener or persistent
port of its own.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any


MAX_CHILDREN = 10
DEFAULT_CHILDREN = 3
DEFAULT_TIMEOUT_SECONDS = 180
MAX_TIMEOUT_SECONDS = 300
MAX_PROMPT_BYTES = 64 * 1024
MAX_MODEL_BYTES = 128
MAX_OUTPUT_BYTES = 256 * 1024
CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000)
CREATE_NEW_PROCESS_GROUP = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0x00000200)


def _truncate(value: str) -> str:
    if len(value.encode("utf-8", errors="replace")) <= MAX_OUTPUT_BYTES:
        return value.strip()
    encoded = value.encode("utf-8", errors="replace")[:MAX_OUTPUT_BYTES]
    trimmed = encoded.decode("utf-8", errors="ignore").rstrip()
    return f"{trimmed}\n[output truncated at {MAX_OUTPUT_BYTES} bytes]"


def _where_first(*names: str) -> str | None:
    for name in names:
        path = shutil.which(name)
        if path:
            return path
    return None


def _npm_node_entry(program: Path) -> list[str] | None:
    if program.suffix.lower() not in {".cmd", ".bat"}:
        return None
    script = (
        program.parent
        / "node_modules"
        / "@moonshot-ai"
        / "kimi-code"
        / "dist"
        / "main.mjs"
    )
    if not script.is_file():
        return None
    node = program.parent / "node.exe"
    return [str(node if node.is_file() else (_where_first("node.exe", "node") or "node")), str(script)]


def _resolve_kimi_command() -> list[str]:
    configured = os.environ.get("KIMICHATCLI_KIMI_PATH") or os.environ.get("KIMI_PATH")
    candidates = [configured] if configured else []
    candidates.extend(
        candidate
        for candidate in (
            _where_first("kimi.cmd"),
            _where_first("kimi.exe"),
            _where_first("kimi"),
        )
        if candidate
    )

    for raw in candidates:
        if not raw:
            continue
        program = Path(raw)
        node_entry = _npm_node_entry(program)
        if node_entry:
            return node_entry
        if program.suffix.lower() in {".cmd", ".bat"}:
            return ["cmd.exe", "/d", "/s", "/c", str(program)]
        return [str(program)]

    return ["kimi.cmd"]


def _validate(arguments: dict[str, Any]) -> tuple[str, int, str, str | None, Path, int]:
    prompt = arguments.get("prompt")
    if not isinstance(prompt, str) or not prompt.strip():
        raise ValueError("kimi_swarm_spawn requires a non-empty string prompt.")
    prompt = prompt.strip()
    if len(prompt.encode("utf-8")) > MAX_PROMPT_BYTES:
        raise ValueError(f"prompt exceeds the {MAX_PROMPT_BYTES}-byte limit.")

    count = arguments.get("count", DEFAULT_CHILDREN)
    if isinstance(count, bool) or not isinstance(count, int) or not 1 <= count <= MAX_CHILDREN:
        raise ValueError(f"count must be between 1 and {MAX_CHILDREN}.")

    agent = arguments.get("agent", "explore")
    if agent not in {"coder", "explore", "plan"}:
        raise ValueError("agent must be one of: coder, explore, plan.")

    model = arguments.get("model")
    if model is not None:
        if not isinstance(model, str) or len(model.strip().encode("utf-8")) > MAX_MODEL_BYTES:
            raise ValueError(f"model id exceeds the {MAX_MODEL_BYTES}-byte limit.")
        model = model.strip() or None

    timeout_seconds = arguments.get("timeout_seconds", DEFAULT_TIMEOUT_SECONDS)
    if (
        isinstance(timeout_seconds, bool)
        or not isinstance(timeout_seconds, int)
        or not 15 <= timeout_seconds <= MAX_TIMEOUT_SECONDS
    ):
        raise ValueError(f"timeout_seconds must be between 15 and {MAX_TIMEOUT_SECONDS}.")

    raw_directory = arguments.get("working_directory")
    working_directory = Path(raw_directory).expanduser() if isinstance(raw_directory, str) else Path.cwd()
    working_directory = working_directory.resolve()
    if not working_directory.is_dir():
        raise ValueError(f"working_directory does not exist or is not a directory: {working_directory}")

    return prompt, count, agent, model, working_directory, timeout_seconds


def _child_prompt(index: int, total: int, prompt: str) -> str:
    return (
        f"You are worker {index}/{total} in a parallel Kimi CLI fan-out. "
        "Work independently on the task below. Do not call MCP tools, do not "
        "create nested workers, and do not wait for another worker. Return a "
        "concise evidence-based report with findings, decisions, and unresolved "
        "issues.\n\nTASK:\n"
        f"{prompt}"
    )


def _terminate_tree(process_id: int) -> None:
    try:
        subprocess.run(
            ["taskkill.exe", "/PID", str(process_id), "/T", "/F"],
            capture_output=True,
            timeout=10,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        pass


def _run_worker(
    index: int,
    total: int,
    prompt: str,
    agent: str,
    model: str | None,
    working_directory: Path,
    timeout_seconds: int,
) -> dict[str, Any]:
    started = time.perf_counter()
    args: list[str] = []
    if model:
        args.extend(["--model", model])
    args.extend(
        [
            "--agent",
            agent,
            "-p",
            _child_prompt(index, total, prompt),
            "--output-format",
            "text",
        ]
    )
    command = _resolve_kimi_command() + args
    environment = os.environ.copy()
    environment.update(
        {
            "KIMICHATCLI_SWARM_CHILD": "1",
            "KIMICHATCLI_SWARM_PARENT_PID": str(os.getpid()),
            "PYTHONIOENCODING": "utf-8",
        }
    )

    try:
        process = subprocess.Popen(
            command,
            cwd=str(working_directory),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            env=environment,
            creationflags=CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
        )
    except OSError as error:
        return {
            "index": index,
            "status": "failed",
            "exit_code": None,
            "error": f"spawn Kimi CLI: {error}",
            "elapsed_ms": round((time.perf_counter() - started) * 1000),
        }

    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout_seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
        _terminate_tree(process.pid)
        stdout, stderr = process.communicate()

    elapsed_ms = round((time.perf_counter() - started) * 1000)
    success = not timed_out and process.returncode == 0
    return {
        "index": index,
        "status": "timed_out" if timed_out else "completed" if success else "failed",
        "exit_code": process.returncode,
        "stdout": _truncate(stdout),
        "stderr": _truncate(stderr),
        "elapsed_ms": elapsed_ms,
    }


def run_fanout(arguments: dict[str, Any]) -> dict[str, Any]:
    prompt, count, agent, model, working_directory, timeout_seconds = _validate(arguments)
    started = time.perf_counter()
    with ThreadPoolExecutor(max_workers=count, thread_name_prefix="kimi-swarm") as executor:
        futures = [
            executor.submit(
                _run_worker,
                index,
                count,
                prompt,
                agent,
                model,
                working_directory,
                timeout_seconds,
            )
            for index in range(1, count + 1)
        ]
        results = [future.result() for future in futures]

    completed = sum(result.get("status") == "completed" for result in results)
    return {
        "status": "completed" if completed == count else "completed_with_errors",
        "backend": "python",
        "requested": count,
        "completed": completed,
        "failed": count - completed,
        "agent": agent,
        "model": model or "<kimi-cli-default>",
        "working_directory": str(working_directory),
        "elapsed_ms": round((time.perf_counter() - started) * 1000),
        "results": results,
    }


def main() -> int:
    try:
        payload = json.load(sys.stdin)
        if not isinstance(payload, dict):
            raise ValueError("backend request must be a JSON object.")
        result = run_fanout(payload)
    except Exception as error:  # The controller receives a structured error instead of a broken pipe.
        result = {"status": "error", "backend": "python", "error": str(error)}
    sys.stdout.write(json.dumps(result, ensure_ascii=False, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
