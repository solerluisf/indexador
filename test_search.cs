using System;
using System.Runtime.InteropServices;
using System.Text;

class TestSearch
{
    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    static extern uint pdf_api_version();

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    static extern int pdf_create_registry(byte[] dir);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    static extern int pdf_search_all(byte[] query, uint limit, uint offset, byte[] outJson, ref uint outLen);

    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    static extern int pdf_last_error(byte[] buf, ref uint len);

    static void Main()
    {
        var dir = Encoding.UTF8.GetBytes("C:\\Users\\Magnesium\\Documents\\Software\\Indexador\\Dev\\PdfExplorer\\bin\\Debug\\net8.0-windows10.0.17763.0\\registry\0");
        Console.WriteLine("API version: {0}", pdf_api_version());
        var rc = pdf_create_registry(dir);
        Console.WriteLine("create_registry: {0}", rc);

        var query = Encoding.UTF8.GetBytes("pattern\0");
        var buf = new byte[65536];
        uint len = (uint)buf.Length;
        rc = pdf_search_all(query, 10, 0, buf, ref len);
        Console.WriteLine("search_all rc={0} len={1}", rc, len);
        if (len > 0) Console.WriteLine("Result: {0}", Encoding.UTF8.GetString(buf, 0, (int)len));
        if (rc != 0)
        {
            var ebuf = new byte[1024];
            uint elen = 1024;
            pdf_last_error(ebuf, ref elen);
            if (elen > 0) Console.WriteLine("Error: {0}", Encoding.UTF8.GetString(ebuf, 0, (int)elen));
        }
    }
}
