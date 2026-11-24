# Tira o icone (PNG 256 px) do .exe de cada app da config para kcu-janelas-icones\<id>.png,
# ao lado da config. A pagina mostra-o na barra, no separador e enquanto a app abre.
param([string]$Config = (Join-Path $PSScriptRoot 'kcu-janelas.json'))
Add-Type -ReferencedAssemblies System.Drawing @"
using System;using System.Runtime.InteropServices;using System.Drawing;
[ComImport,Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"),InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IShellItemImageFactory{void GetImage(SIZE s,int f,out IntPtr h);}
[StructLayout(LayoutKind.Sequential)]public struct SIZE{public int cx,cy;}
public static class KcuIcone{
[DllImport("shell32.dll",CharSet=CharSet.Unicode,PreserveSig=false)]static extern void SHCreateItemFromParsingName(string p,IntPtr b,[MarshalAs(UnmanagedType.LPStruct)]Guid r,[MarshalAs(UnmanagedType.Interface)]out IShellItemImageFactory f);
[DllImport("gdi32.dll")]static extern bool DeleteObject(IntPtr h);
public static void Guardar(string exe,string png){IShellItemImageFactory f;SHCreateItemFromParsingName(exe,IntPtr.Zero,new Guid("bcc18b79-ba16-442f-80c4-8a59c30c463b"),out f);IntPtr h;f.GetImage(new SIZE{cx=256,cy=256},1,out h);
var b=Image.FromHbitmap(h);var d=b.LockBits(new Rectangle(0,0,b.Width,b.Height),System.Drawing.Imaging.ImageLockMode.ReadOnly,b.PixelFormat);
var o=new Bitmap(b.Width,b.Height,d.Stride,System.Drawing.Imaging.PixelFormat.Format32bppArgb,d.Scan0);o.Save(png,System.Drawing.Imaging.ImageFormat.Png);b.UnlockBits(d);DeleteObject(h);}}
"@
$cfg = Get-Content $Config -Raw | ConvertFrom-Json
$dir = Join-Path (Split-Path $Config) 'kcu-janelas-icones'
New-Item -ItemType Directory -Force $dir | Out-Null
foreach ($p in $cfg.apps.PSObject.Properties) {
  try { [KcuIcone]::Guardar($p.Value.exe, (Join-Path $dir "$($p.Name).png")); "$($p.Name): ok" }
  catch { "$($p.Name): falhou ($($_.Exception.Message))" }
}
