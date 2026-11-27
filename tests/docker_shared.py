"""Verify that two clients share one Dockerized MCP server backed by real SQL Server."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
from urllib.request import ProxyHandler, Request, build_opener
import uuid

from sql_server_live import ROOT, fixture


URL = "http://127.0.0.1:8765/mcp"


def verify_client(token, name):
    opener = build_opener(ProxyHandler({}))

    def post(method, request_id, params):
        data = {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        headers = {
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2025-06-18",
        }
        request = Request(URL, data=json.dumps(data).encode(), headers=headers)
        with opener.open(request, timeout=20) as response:
            result = json.load(response)
        assert result["id"] == request_id and "error" not in result, result
        return result["result"]

    initialize = post("initialize", 1, {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": name, "version": "1"},
    })
    assert initialize["serverInfo"]["name"] == "mssql-mcp"
    tools = post("tools/list", 2, {})["tools"]
    # Mirrors Mode::Read in src/server.rs: the read surface exposes every read
    # tool and no mutation.
    assert {tool["name"] for tool in tools} == {
        "list_tables", "describe_table", "select_rows", "explore_schema", "schema_map",
        "object_dependencies", "routine_definition", "dependency_columns",
        "search_objects", "table_stats", "sample_rows",
    }
    result = post("tools/call", 3, {
        "name": "select_rows", "arguments": {
            "table": {"name": "Items"}, "columns": ["id"], "filters": {"id": 1000},
        },
    })
    assert result["structuredContent"]["rows"] == [["1000"]], result


def verify_docker(image, scratch):
    project = f"mssql-mcp-smoke-{uuid.uuid4().hex[:10]}"
    smoke_image = f"{project}:smoke"
    override = Path(scratch) / "compose.yaml"
    override.write_text(f"services:\n  mssql-mcp:\n    image: {smoke_image}\n", encoding="utf-8")
    token = secrets.token_hex(32)
    reader = secrets.token_hex(22) + "Aa1!"
    env = {key: value for key, value in os.environ.items() if not key.startswith(("MSSQL_", "COMPOSE_"))}
    env.update(
        MSSQL_HOST="sql", MSSQL_DATABASE="McpFixture", MSSQL_USER="mcp_reader",
        MSSQL_PASSWORD=reader, MSSQL_MCP_TOKEN=token, MSSQL_TRUST_SERVER_CERTIFICATE="true",
    )

    def compose(*args, timeout=45, capture=False):
        return subprocess.run(
            ["docker", "compose", "--env-file", os.devnull, "--project-name", project,
             "--file", str(ROOT / "compose.yaml"), "--file", str(override), *args],
            cwd=ROOT, env=env, timeout=timeout, check=True, capture_output=capture, text=True,
        )

    def service_containers():
        ids = compose("ps", "-q", "--all", "mssql-mcp", capture=True).stdout.split()
        assert len(ids) == 1, f"expected exactly one MCP container, got {ids}"
        return ids[0]

    try:
        compose("up", "--detach", "--build", "--wait", "--wait-timeout", "20", timeout=1200)
        first_id = service_containers()
        compose("up", "--detach", "--no-build", "--wait", "--wait-timeout", "20")
        second_id = service_containers()
        print(f"compose up twice: {first_id} -> {second_id} (one container is the invariant)")
        info = subprocess.run(
            ["docker", "inspect", "--format", "{{.Config.User}} {{.HostConfig.ReadonlyRootfs}}", second_id],
            check=True, capture_output=True, text=True, timeout=10,
        ).stdout.split()
        assert len(info) == 2 and info[1] == "true" and info[0].split(":")[0] not in ("0", "root"), info
        ports = compose("port", "mssql-mcp", "8765", capture=True).stdout.strip()
        assert ports == "127.0.0.1:8765", ports
        with fixture(image, network=f"{project}_default", reader_password=reader):
            with ThreadPoolExecutor(max_workers=2) as pool:
                results = [pool.submit(verify_client, token, name) for name in ("client-a", "client-b")]
                for result in results:
                    result.result()
            service_containers()
        print("PASS: two authenticated clients queried SQL Server through one loopback-only, non-root container")
    finally:
        compose("down", "--remove-orphans")
        subprocess.run(["docker", "image", "rm", smoke_image], timeout=15, capture_output=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", default="mcr.microsoft.com/mssql/server:2022-latest")
    with tempfile.TemporaryDirectory(prefix="mssql-mcp-smoke-") as scratch:
        verify_docker(parser.parse_args().image, scratch)
