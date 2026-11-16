// Independent synthetic VHD fixtures, all files below the explicitly checked project folder.
using DiscUtils.Vhd;
using DiscUtils.Streams;
using System.IO.Compression;
using System.Security.Cryptography;
var root = Path.GetFullPath(Environment.CurrentDirectory);
if (!File.Exists(Path.Combine(root, "src/vhdImage.ts"))) throw new Exception("Not in DiskStudio workspace");
var output = Path.Combine(root, ".vm/vhd-chain-fixtures");
foreach (var dir in new[] { root, Path.Combine(root, ".vm"), Path.Combine(root, "tests/fixtures") })
    if ((File.GetAttributes(dir) & FileAttributes.ReparsePoint) != 0) throw new Exception("Linked fixture folder");
if (Directory.Exists(output)) throw new Exception("Fixture output already exists; refusing overwrite");
Directory.CreateDirectory(output);
var raw = new byte[8 * 1024 * 1024];
using (var zip = new GZipStream(File.OpenRead(Path.Combine(root,"tests/fixtures/vmdk-raw.gz")), CompressionMode.Decompress)) zip.ReadExactly(raw);
var timestamp = new DateTime(2026, 1, 2, 3, 4, 5, DateTimeKind.Utc);
var names = new[] { "base.vhd", "middle.avhd", "leaf.avhd" };
var layers = new List<DiskImageFile>(); var streams = new List<FileStream>();
var expected = raw.ToArray();
try {
    for (var i = 0; i < names.Length; i++) {
        var stream = new FileStream(Path.Combine(output,names[i]), FileMode.CreateNew, FileAccess.ReadWrite, FileShare.None);
        streams.Add(stream);
        var layer = i == 0 ? DiskImageFile.InitializeDynamic(stream, Ownership.None, raw.Length) :
            DiskImageFile.InitializeDifferencing(stream, Ownership.None, layers[i-1], @"C:\SyntheticFixtures\" + names[i-1], names[i-1], timestamp);
        layers.Add(layer);
        using var parent = i == 0 ? null : new ZeroStream(raw.Length);
        using var content = layer.OpenContent(parent, Ownership.None);
        if (i == 0) content.Write(raw);
        else {
            // A child-sector zero overrides parent nonzero data; untouched EBRs inherit.
            var offsets = i == 1 ? new[] { 8192, 2 * 1024 * 1024 - 512, 7 * 1024 * 1024 } : new[] { 8704, 2 * 1024 * 1024, 8 * 1024 * 1024 - 512 };
            foreach(var at in offsets) {
                var bytes = new byte[512]; if(at != 8192) Array.Fill(bytes,(byte)(i * 51));
                content.Position=at; content.Write(bytes); bytes.CopyTo(expected,at);
            }
        }
    }
} finally { foreach(var l in layers) l.Dispose(); foreach(var s in streams) s.Dispose(); }
foreach(var name in names) File.SetLastWriteTimeUtc(Path.Combine(output,name), timestamp);
// Explicit read-only streams and ordered layers: never follow even fixture locator paths.
layers.Clear(); streams.Clear();
try {
    foreach(var name in names.Reverse()) { var stream = File.OpenRead(Path.Combine(output,name)); streams.Add(stream); layers.Add(new DiskImageFile(stream, Ownership.None)); }
    using var disk = new Disk(layers, Ownership.None);
    var actual = new byte[expected.Length]; disk.Content.ReadExactly(actual);
    if (!actual.SequenceEqual(expected)) throw new Exception("Independent chain decode differs");
    Console.WriteLine("DiscUtils full 8 MiB chain decode equals independently patched RAW.");
} finally { foreach(var l in layers) l.Dispose(); foreach(var s in streams) s.Dispose(); }
File.WriteAllBytes(Path.Combine(output,"expected.raw"),expected);
foreach(var name in names.Concat(new[]{"expected.raw"})) {
    var bytes=File.ReadAllBytes(Path.Combine(output,name));
    using(var zip=new GZipStream(new FileStream(Path.Combine(root,"tests/fixtures/vhd-chain-"+name+".gz"),FileMode.CreateNew),CompressionLevel.SmallestSize)) zip.Write(bytes);
    Console.WriteLine($"{name}: {bytes.Length} bytes; SHA256 {Convert.ToHexString(SHA256.HashData(bytes))}");
}
Console.WriteLine("File timestamps: "+timestamp.ToString("O"));
