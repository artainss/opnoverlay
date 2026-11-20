using System;
using System.Collections.Generic;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;

public static class OpnoverlayInstallerIcon
{
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryEx(string path, IntPtr unused, uint flags);
    [DllImport("kernel32.dll", EntryPoint = "FindResourceW", SetLastError = true)]
    private static extern IntPtr FindResource(IntPtr module, IntPtr name, IntPtr type);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LoadResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern int SizeofResource(IntPtr module, IntPtr resource);
    [DllImport("kernel32.dll")]
    private static extern IntPtr LockResource(IntPtr memory);
    [DllImport("kernel32.dll")]
    private static extern bool FreeLibrary(IntPtr module);

    private static byte[] Read(IntPtr module, int type, int name)
    {
        IntPtr resource = FindResource(module, new IntPtr(name), new IntPtr(type));
        if (resource == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        int length = SizeofResource(module, resource);
        IntPtr memory = LoadResource(module, resource);
        if (length <= 0 || memory == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        IntPtr bytes = LockResource(memory);
        if (bytes == IntPtr.Zero) throw new InvalidDataException("Could not read icon bytes");
        byte[] data = new byte[length];
        Marshal.Copy(bytes, data, 0, length);
        return data;
    }

    public static void Save(string executable, string destination)
    {
        const uint LoadAsDataFile = 2;
        IntPtr module = LoadLibraryEx(executable, IntPtr.Zero, LoadAsDataFile);
        if (module == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        try
        {
            byte[] group = Read(module, 14, 1);
            if (group.Length < 6 || BitConverter.ToUInt16(group, 2) != 1)
                throw new InvalidDataException("Invalid icon group header");
            int count = BitConverter.ToUInt16(group, 4);
            if (count == 0 || group.Length != 6 + count * 14)
                throw new InvalidDataException("Invalid icon group length");
            var images = new List<byte[]>(count);
            using (var file = new BinaryWriter(File.Create(destination)))
            {
                file.Write(group, 0, 6);
                int offset = 6 + count * 16;
                for (int index = 0; index < count; index++)
                {
                    int entry = 6 + index * 14;
                    byte[] image = Read(module, 3, BitConverter.ToUInt16(group, entry + 12));
                    if (image.Length != BitConverter.ToUInt32(group, entry + 8))
                        throw new InvalidDataException("Embedded icon image size is inconsistent");
                    file.Write(group, entry, 12);
                    file.Write(offset);
                    offset = checked(offset + image.Length);
                    images.Add(image);
                }
                foreach (byte[] image in images) file.Write(image);
            }
        }
        finally
        {
            FreeLibrary(module);
        }
    }
}
