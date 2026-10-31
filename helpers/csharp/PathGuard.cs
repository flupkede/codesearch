namespace ScipCsharp;

/// <summary>
/// Path validation for the fs operations Aikido SAST 30640677 flags (path
/// traversal, priority 90). The helper is a local build tool whose inputs
/// come from the invoking codesearch binary, not from a remote attacker —
/// these guards enforce exactly the invariants the tool already relies on,
/// failing fast instead of trusting construction.
/// </summary>
internal static class PathGuard
{
    /// <summary>
    /// Normalize a caller-supplied path and require it to resolve to an
    /// absolute location before any fs operation on it.
    /// </summary>
    public static string Validate(string path)
    {
        // Checked on the INPUT: GetFullPath would silently resolve a
        // relative path against an arbitrary CWD — exactly the ambiguity
        // this guard exists to refuse.
        if (!Path.IsPathFullyQualified(path))
        {
            throw new ArgumentException($"path must resolve to an absolute location: {path}");
        }
        return Path.GetFullPath(path);
    }

    /// <summary>
    /// Assert <paramref name="candidate"/> stays inside
    /// <paramref name="baseDir"/> and carries no unresolved <c>..</c>
    /// segment; returns the normalized full path.
    /// </summary>
    public static string Inside(string baseDir, string candidate)
    {
        var fullBase = Path.GetFullPath(baseDir)
            .TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        var full = Path.GetFullPath(candidate);
        var comparison = OperatingSystem.IsWindows()
            ? StringComparison.OrdinalIgnoreCase
            : StringComparison.Ordinal;
        var contained = string.Equals(full, fullBase, comparison)
            || full.StartsWith(fullBase + Path.DirectorySeparatorChar, comparison);
        if (!contained)
        {
            throw new ArgumentException($"path escapes {fullBase}: {candidate}");
        }
        var segments = candidate.Split(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
        if (segments.Contains(".."))
        {
            throw new ArgumentException($"path contains an unresolved '..' segment: {candidate}");
        }
        return full;
    }

    /// <summary>
    /// Non-throwing variant of <see cref="Inside"/> for per-entry loops:
    /// returns null plus the rejection reason instead of aborting the run —
    /// degrade the one entry, never the whole pass (see the Program.cs
    /// sln-projection call site).
    /// </summary>
    public static string? TryInside(string baseDir, string candidate, out string error)
    {
        try
        {
            error = string.Empty;
            return Inside(baseDir, candidate);
        }
        catch (ArgumentException ex)
        {
            error = ex.Message;
            return null;
        }
    }
}
