namespace PdfExplorer;

internal static class LayoutConstants
{
    public const double BorderPadding = 5.0;
    public const double BorderThickness = 1.0;
    public const double TextBlockHeight = 18.0;
    public const double TextBlockMargin = 4.0;
    public const double BorderMarginBottom = 10.0;

    public static double AvailWidth(double viewportWidth)
        => viewportWidth - BorderPadding * 2 - BorderThickness * 2;

    public static double ImageHeight(double availW, double imgW, double imgH)
    {
        if (imgW <= 0 || imgH <= 0) return 0;
        return imgH * Math.Min(1.0, availW / imgW);
    }

    public static double TotalItemHeight(double availW, double imgW, double imgH)
        => BorderThickness + BorderPadding + TextBlockHeight + TextBlockMargin
         + ImageHeight(availW, imgW, imgH)
         + BorderPadding + BorderThickness + BorderMarginBottom;

    public static double WordOffsetWithinItem(double availW, double imgW, double imgH, double normalizedY)
        => BorderThickness + BorderPadding + TextBlockHeight + TextBlockMargin
         + ImageHeight(availW, imgW, imgH) * normalizedY;
}
