# Test the pipeline directly via the DLL
$dllPath = "C:\Users\Magnesium\Documents\Software\Indexador\Dev\PdfExplorer\bin\Debug\net8.0-windows10.0.17763.0\pdf_extractor_capi.dll"

# Load the DLL
Add-Type -Path $dllPath

# Import the function
$MethodDefinition = @"
    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    public static extern int pdf_init();
    
    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    public static extern int pdf_create_registry(IntPtr path);
    
    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    public static extern int pdf_add_collection(IntPtr books_folder);
    
    [DllImport("pdf_extractor_capi.dll", CallingConvention = CallingConvention.Cdecl)]
    public static extern int pdf_index_collection(long coll_id, uint flags, IntPtr progress);
"@

$type = Add-Type -MemberDefinition $MethodDefinition -Name "PdfExtractor" -PassThru

# Initialize
$type::pdf_init()

# Create registry
$regPath = [System.Runtime.InteropServices.Marshal]::StringToHGlobalAnsi("C:\Users\Magnesium\Documents\Software\Indexador\Dev\test_registry")
$regId = $type::pdf_create_registry($regPath)
Write-Host "Registry ID: $regId"

# Add collection
$booksPath = [System.Runtime.InteropServices.Marshal]::StringToHGlobalAnsi("C:\Users\Magnesium\Documents\Java")
$collId = $type::pdf_add_collection($booksPath)
Write-Host "Collection ID: $collId"

# Index collection
$result = $type::pdf_index_collection($collId, 0, [IntPtr]::Zero)
Write-Host "Index result: $result"
