namespace DshDesktop.WinUI;

/// <summary>
/// The three screens the shell draws itself, expressed as data.
///
/// The vocabulary matches <c>src-ui/error.html</c> in the Tauri shell and the
/// <c>error</c> codes emitted by <c>crates/dsh-ffi</c>, so both shells say the
/// same thing about the same failure.
/// </summary>
internal sealed record ErrorContent(
    string Title,
    string Body,
    string Command,
    IReadOnlyList<string> Steps);

internal static class ErrorCatalog
{
    public static ErrorContent For(string kind, string? detail)
    {
        return kind switch
        {
            "dsh-not-found" => new ErrorContent(
                "找不到 dsh",
                "壳在你的 PATH 与常见安装位置里都没有找到可用的 dsh。",
                "npm install -g @deepseek-ai/dsh",
                new[]
                {
                    "安装 dsh：npm install -g @deepseek-ai/dsh",
                    "或用 --dsh-path 指向已有的 dsh，或写入配置的 [dsh] path。",
                }),
            "version" => new ErrorContent(
                "dsh 版本过低",
                "已安装的 dsh 不在支持的版本范围内（>= 0.2.0，< 1.0.0）。",
                "npm install -g @deepseek-ai/dsh@latest",
                new[] { "升级 dsh：npm install -g @deepseek-ai/dsh@latest" }),
            "spawn" => new ErrorContent(
                "无法启动 dsh",
                "进程无法创建。请看下面的底层报错，通常是权限或路径问题。",
                "dsh --version",
                new[] { "确认 dsh 能在这个用户下直接运行；", "再点一次“重试连接”。" }),
            "auth" => new ErrorContent(
                "认证失败",
                "dsh 报告的地址无法使用，一次性 token 没有被消费。",
                "dsh web --host 127.0.0.1 --port 0",
                new[]
                {
                    "在运行 dsh 的终端里复制完整的 `dsh web:` 行，用 --url 重新附着；",
                    "或改用托管模式，让壳自己启动 dsh。",
                }),
            "ready" => new ErrorContent(
                "dsh 没有报告就绪地址",
                "dsh 在超时前没有打印 `dsh web:` 行，或者该行无法解析。",
                "dsh web --host 127.0.0.1 --port 0",
                new[]
                {
                    "在终端里手动运行下列命令，查看 dsh 自身的报错；",
                    "修好之后再回来点“重试连接”。",
                }),
            _ => new ErrorContent(
                "启动失败",
                detail ?? "壳自身出现了未分类的错误。",
                "dsh --version",
                new[] { "打开日志目录查看完整报错；", "然后点“重试连接”。" }),
        };
    }
}
