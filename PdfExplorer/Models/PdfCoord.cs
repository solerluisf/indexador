namespace PdfExplorer.Models;

public readonly record struct PdfCoord(double X, double Y)
{
    public static readonly PdfCoord Zero = new(0, 0);
}

public readonly record struct PdfRect(PdfCoord TopLeft, PdfCoord BottomRight)
{
    public double Width => BottomRight.X - TopLeft.X;
    public double Height => BottomRight.Y - TopLeft.Y;
    public PdfCoord Center => new(
        (TopLeft.X + BottomRight.X) / 2.0,
        (TopLeft.Y + BottomRight.Y) / 2.0);

    public static PdfRect FromLtrb(double left, double top, double right, double bottom)
        => new(new PdfCoord(left, top), new PdfCoord(right, bottom));
}
