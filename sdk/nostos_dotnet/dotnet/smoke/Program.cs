// Live-replication E2E for the .NET (C#) binding against the shared spine
// server (crates/nostos-infra/examples/e2e_server) — the C# mirror of
// sdk/nostos_node/smoke_live.cjs and crates/nostos-client/tests/e2e_live_replication.rs.
//
// 1. PUSH:  POST /push injects a `tasks` row server-side → real WS replication
//           → the C# binding applies it → Query() reads it.
// 2. ECHO:  Write() enqueues a durable write → the spine's echo WriteBack
//           re-emits it → the binding applies its own write → Query() reads it.
//
// Driven by ../scripts/run-dotnet-e2e.sh, which spawns the spine and exports
// NOSTOS_E2E_PORT. The host libnostos_dotnet.dylib is copied next to this
// assembly by Smoke.csproj so DllImport("nostos_dotnet") resolves.

using System;
using System.IO;
using System.Net.Http;
using System.Text;
using System.Text.Json;
using System.Threading;
using uniffi.nostos;

namespace Nostos.Smoke;

internal static class Program
{
    private const string Table = "tasks";

    private static int Main()
    {
        string? portEnv = Environment.GetEnvironmentVariable("NOSTOS_E2E_PORT");
        if (!int.TryParse(portEnv, out int port) || port <= 0)
        {
            Console.Error.WriteLine("[dotnet-e2e] FAIL: NOSTOS_E2E_PORT not set / invalid");
            return 1;
        }

        string wsUrl = $"ws://127.0.0.1:{port}/sync";
        // PID-unique DB path so a stale file from a prior run can't false-positive.
        string dbPath = Path.Combine(Path.GetTempPath(), $"nostos-dotnet-e2e-{Environment.ProcessId}.sqlite");
        TryDelete(dbPath);

        int exitCode = 1;
        NostosClient? client = null;
        try
        {
            client = new NostosClient(wsUrl, token: null, dbPath: dbPath);
            Console.WriteLine($"[dotnet-e2e] client url={wsUrl}");
            client.Connect();
            client.Subscribe(Table);
            Console.WriteLine("[dotnet-e2e] subscribed to tasks");
            // Let the session register with the fan-out service (the spine only
            // delivers to sessions registered at fan-out time). Mirrors node's 500ms.
            Thread.Sleep(500);

            using var http = new HttpClient();

            // ---- direction 1: server PUSH → on-device query ----
            string pushBody = "{\"pk\":\"dotnet-push\",\"payload\":{\"title\":\"from-server\",\"status\":\"open\",\"priority\":\"5\"}}";
            var pushResp = http.PostAsync(
                $"http://127.0.0.1:{port}/push",
                new StringContent(pushBody, Encoding.UTF8, "application/json")).Result;
            if (!pushResp.IsSuccessStatusCode)
                throw new Exception($"POST /push non-200: {(int)pushResp.StatusCode}");
            Console.WriteLine("[dotnet-e2e] POST /push ok");

            string pushedPk = PollRow(client,
                "SELECT pk FROM nostos_data WHERE table_name = 'tasks' AND pk = 'dotnet-push'",
                TimeSpan.FromSeconds(8));
            if (pushedPk != "dotnet-push")
                throw new Exception($"unexpected pushed pk: {pushedPk}");
            Console.WriteLine($"[dotnet-e2e] PUSH_OK: {pushedPk}");

            // ---- direction 2: client WRITE → server echo → on-device query ----
            string payloadJson = "{\"title\":\"from-client\",\"status\":\"open\",\"priority\":\"5\"}";
            client.Write(Table, "upsert", "dotnet-echo", payloadJson);
            Console.WriteLine("[dotnet-e2e] write() enqueued (dotnet-echo)");

            string echoedPk = PollRow(client,
                "SELECT pk FROM nostos_data WHERE table_name = 'tasks' AND pk = 'dotnet-echo'",
                TimeSpan.FromSeconds(8));
            if (echoedPk != "dotnet-echo")
                throw new Exception($"unexpected echoed pk: {echoedPk}");
            Console.WriteLine($"[dotnet-e2e] ECHO_OK: {echoedPk}");

            Console.WriteLine("[dotnet-e2e] DONE");
            exitCode = 0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[dotnet-e2e] FAIL: {e}");
        }
        finally
        {
            try { client?.Dispose(); } catch { /* best-effort teardown */ }
            TryDelete(dbPath);
        }
        return exitCode;
    }

    // Poll Query() until a row appears or the deadline elapses; returns the
    // first row's `pk` (the projection is a single column). UniFFI returns
    // rows as a JSON array string — same shape node parses with JSON.parse.
    private static string PollRow(NostosClient c, string sql, TimeSpan timeout)
    {
        DateTime end = DateTime.UtcNow + timeout;
        string lastRows = "[]";
        while (DateTime.UtcNow < end)
        {
            lastRows = c.Query(sql);
            using var doc = JsonDocument.Parse(lastRows);
            if (doc.RootElement.ValueKind == JsonValueKind.Array &&
                doc.RootElement.GetArrayLength() > 0)
            {
                var first = doc.RootElement[0];
                if (first.TryGetProperty("pk", out var pkEl) &&
                    pkEl.ValueKind == JsonValueKind.String)
                {
                    return pkEl.GetString() ?? string.Empty;
                }
            }
            Thread.Sleep(100);
        }
        throw new Exception(
            $"row never became queryable within {timeout.TotalSeconds}s (last rows: {lastRows})");
    }

    private static void TryDelete(string path)
    {
        try { File.Delete(path); } catch { /* absent is fine */ }
    }
}
