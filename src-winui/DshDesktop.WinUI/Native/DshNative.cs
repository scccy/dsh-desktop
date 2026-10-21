using System.Runtime.InteropServices;
using System.Text.Json;
using System.Text.Json.Serialization;
using Microsoft.UI.Dispatching;

namespace DshDesktop.WinUI.Native;

/// <summary>
/// Raw P/Invoke surface of <c>dsh_ffi.dll</c> (crate <c>crates/dsh-ffi</c>).
///
/// Every string crossing this boundary is UTF-8. Pointers returned as
/// <see cref="IntPtr"/> belong to us and must go through
/// <see cref="Methods.dsh_string_free"/>; pointers handed to us in a callback are
/// only valid for the duration of that call.
/// </summary>
internal static class Methods
{
    private const string Library = "dsh_ffi.dll";

    [DllImport(Library, ExactSpelling = true)]
    internal static extern uint dsh_ffi_abi_version();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern IntPtr dsh_shell_version();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern void dsh_string_free(IntPtr value);

    [DllImport(Library, ExactSpelling = true)]
    internal static extern IntPtr dsh_last_error();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern IntPtr dsh_diagnose();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern int dsh_set_event_sink(EventCallback? callback, IntPtr user);

    [DllImport(Library, ExactSpelling = true)]
    internal static extern int dsh_start(IntPtr optionsJson);

    [DllImport(Library, ExactSpelling = true)]
    internal static extern int dsh_stop();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern int dsh_open_logs();

    [DllImport(Library, ExactSpelling = true)]
    internal static extern int dsh_check_update();

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void EventCallback(IntPtr kind, IntPtr payload, IntPtr user);
}

/// <summary>One update-check event pushed by the Rust core.</summary>
public sealed record UpdateState(
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("latest")] string? Latest = null,
    [property: JsonPropertyName("installed")] string? Installed = null,
    [property: JsonPropertyName("newer")] bool Newer = false,
    [property: JsonPropertyName("upgradeCommand")] string? UpgradeCommand = null,
    [property: JsonPropertyName("detail")] string? Detail = null);

/// <summary>One state event pushed by the Rust core.</summary>
public sealed record ShellState(
    [property: JsonPropertyName("state")] string State,
    [property: JsonPropertyName("error")] string? Error = null,
    [property: JsonPropertyName("detail")] string? Detail = null,
    [property: JsonPropertyName("url")] string? Url = null,
    [property: JsonPropertyName("webUrl")] string? WebUrl = null,
    [property: JsonPropertyName("pid")] int? Pid = null,
    [property: JsonPropertyName("profile")] string? Profile = null,
    [property: JsonPropertyName("dshVersion")] string? DshVersion = null)
{
    /// <summary>Wire name of the event; always <c>state</c> in ABI version 1.</summary>
    public const string Kind = "state";
}

/// <summary>Result of <c>dsh_diagnose</c>.</summary>
public sealed record ShellDiagnostics
{
    [JsonPropertyName("shellVersion")] public string? ShellVersion { get; init; }
    [JsonPropertyName("abiVersion")] public uint AbiVersion { get; init; }
    [JsonPropertyName("configPath")] public string? ConfigPath { get; init; }
    [JsonPropertyName("logDir")] public string? LogDir { get; init; }
    [JsonPropertyName("dshFound")] public bool DshFound { get; init; }
    [JsonPropertyName("dshPath")] public string? DshPath { get; init; }
    [JsonPropertyName("dshVersion")] public string? DshVersion { get; init; }
    [JsonPropertyName("dshVersionSupported")] public bool? DshVersionSupported { get; init; }
    [JsonPropertyName("profile")] public string? Profile { get; init; }
    [JsonPropertyName("configuredPort")] public ushort ConfiguredPort { get; init; }
    [JsonPropertyName("nodeVersion")] public string? NodeVersion { get; init; }
    [JsonPropertyName("error")] public string? Error { get; init; }
}

/// <summary>Options handed to <c>dsh_start</c>.</summary>
public sealed record StartOptions
{
    [JsonPropertyName("profile")] public string? Profile { get; init; }
    [JsonPropertyName("port")] public ushort? Port { get; init; }
    [JsonPropertyName("dshPath")] public string? DshPath { get; init; }
    [JsonPropertyName("timeoutMs")] public ulong? TimeoutMs { get; init; }
}

/// <summary>
/// Source-generated serialization for the three shapes that cross the bridge.
///
/// Trimming removes reflection metadata, so the reflection-based
/// <c>JsonSerializer</c> overloads would work in Debug and fail in the trimmed
/// release build. Generating the contracts at compile time removes that trap.
/// </summary>
[JsonSourceGenerationOptions(GenerationMode = JsonSourceGenerationMode.Metadata)]
[JsonSerializable(typeof(ShellState))]
[JsonSerializable(typeof(ShellDiagnostics))]
[JsonSerializable(typeof(StartOptions))]
[JsonSerializable(typeof(UpdateState))]
internal sealed partial class ShellJsonContext : JsonSerializerContext;

/// <summary>
/// Managed face of the bridge. Owns the callback lifetime and marshals every
/// event onto the UI thread, so subscribers never touch XAML from a Rust worker
/// thread.
/// </summary>
public sealed class DshBridge : IDisposable
{
    /// <summary>The ABI this C# code was written against.</summary>
    public const uint ExpectedAbiVersion = 2;

    // Rooted for the lifetime of the process: the native side keeps the raw
    // function pointer, so the delegate must not be collectible.
    private static Methods.EventCallback? _callback;

    private readonly DispatcherQueue _dispatcher;
    private bool _disposed;

    public DshBridge(DispatcherQueue dispatcher)
    {
        _dispatcher = dispatcher;
    }

    /// <summary>Raised on the UI thread for every state event.</summary>
    public event Action<ShellState>? StateChanged;

    /// <summary>Raised on the UI thread for every update-check event.</summary>
    public event Action<UpdateState>? UpdateChecked;

    /// <summary>
    /// Installs the event sink and verifies that the loaded library speaks the
    /// ABI this code expects. A mismatch is a packaging bug, not a user error.
    /// </summary>
    public uint Connect()
    {
        var abi = Methods.dsh_ffi_abi_version();
        if (abi != ExpectedAbiVersion)
        {
            throw new InvalidOperationException(
                $"dsh_ffi.dll speaks ABI {abi}, this shell expects {ExpectedAbiVersion}.");
        }

        _callback ??= OnNativeEvent;
        Methods.dsh_set_event_sink(_callback, IntPtr.Zero);
        return abi;
    }

    /// <summary>Shell version, read once from the core.</summary>
    public string ReadShellVersion()
    {
        var pointer = Methods.dsh_shell_version();
        try
        {
            return pointer == IntPtr.Zero
                ? "unknown"
                : Marshal.PtrToStringUTF8(pointer) ?? "unknown";
        }
        finally
        {
            Methods.dsh_string_free(pointer);
        }
    }

    /// <summary>Machine facts for the settings page.</summary>
    public ShellDiagnostics Diagnose()
    {
        var pointer = Methods.dsh_diagnose();
        try
        {
            var json = pointer == IntPtr.Zero ? "{}" : Marshal.PtrToStringUTF8(pointer) ?? "{}";
            return JsonSerializer.Deserialize(json, ShellJsonContext.Default.ShellDiagnostics) ?? new ShellDiagnostics();
        }
        finally
        {
            Methods.dsh_string_free(pointer);
        }
    }

    /// <summary>
    /// Starts the managed connection. Returns <c>null</c> on success, or the
    /// core's error text when the call was rejected outright; the asynchronous
    /// outcome arrives through <see cref="StateChanged"/>.
    /// </summary>
    public string? Start(StartOptions options)
    {
        var json = JsonSerializer.Serialize(options, ShellJsonContext.Default.StartOptions);
        var pointer = Marshal.StringToCoTaskMemUTF8(json);
        try
        {
            var code = Methods.dsh_start(pointer);
            return code == 0 ? null : TakeLastError() ?? $"dsh_start failed with code {code}";
        }
        finally
        {
            Marshal.FreeCoTaskMem(pointer);
        }
    }

    /// <summary>Stops the sponsored child and invalidates in-flight work.</summary>
    public void Stop() => Methods.dsh_stop();

    /// <summary>Opens the log directory in Explorer.</summary>
    public void OpenLogs() => Methods.dsh_open_logs();

    /// <summary>
    /// Asks npm for the published <c>dsh</c> version. Returns <c>null</c> when the
    /// probe was accepted (the verdict arrives through <see cref="UpdateChecked"/>),
    /// or the core's error text when one is already running.
    /// </summary>
    public string? CheckUpdate()
    {
        var code = Methods.dsh_check_update();
        return code == 0 ? null : TakeLastError() ?? $"dsh_check_update failed with code {code}";
    }

    /// <summary>Pulls and clears the core's last error text.</summary>
    private static string? TakeLastError()
    {
        var pointer = Methods.dsh_last_error();
        if (pointer == IntPtr.Zero)
        {
            return null;
        }

        try
        {
            return Marshal.PtrToStringUTF8(pointer);
        }
        finally
        {
            Methods.dsh_string_free(pointer);
        }
    }

    private void OnNativeEvent(IntPtr kind, IntPtr payload, IntPtr user)
    {
        // Runs on a Rust worker thread: parse and hand over, never touch XAML.
        var json = payload == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(payload);
        if (string.IsNullOrEmpty(json))
        {
            return;
        }

        // `kind` is the discriminator; today the payload also carries it, so a
        // reader that only has the payload still routes correctly.
        var eventKind = kind == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(kind);
        if (string.Equals(eventKind, "update", StringComparison.Ordinal))
        {
            UpdateState? update;
            try
            {
                update = JsonSerializer.Deserialize(json, ShellJsonContext.Default.UpdateState);
            }
            catch (JsonException)
            {
                return;
            }

            if (update is not null)
            {
                _dispatcher.TryEnqueue(() => UpdateChecked?.Invoke(update));
            }

            return;
        }

        ShellState? state;
        try
        {
            state = JsonSerializer.Deserialize(json, ShellJsonContext.Default.ShellState);
        }
        catch (JsonException)
        {
            return;
        }

        if (state is null)
        {
            return;
        }

        _dispatcher.TryEnqueue(() => StateChanged?.Invoke(state));
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        Methods.dsh_set_event_sink(null, IntPtr.Zero);
        _disposed = true;
    }
}
