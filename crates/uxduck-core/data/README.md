# EGM96 geoid grid

`egm96-15.bin` is the unchanged big-endian unsigned 16-bit raster from GeographicLib's EGM96 15-minute PGM, with only the PGM header removed. Dimensions: 1440 × 721; origin 90 N, 0 E; longitude wraps; row spacing 0.25 degrees. Height in metres is sample × 0.003 − 108.

Source: https://sourceforge.net/projects/geographiclib/files/geoids-distrib/egm96-15.tar.bz2/
NGA's EGM96 data are public domain. GeographicLib's distribution documents maximum bilinear interpolation error of 1.152 m (not total measurement/model accuracy).

Original PGM SHA-256: `2a12f13b6df65cdea52432c7fa1b43f34b007148eb817bf032af5af710905caa`
Raster SHA-256: `41a42df6747fcbeac0c28508a5b8748648c542530934640232a66a0a32d246e4`

Used only to convert WGS84 ellipsoid height to EGM96 mean-sea-level height. Pressure altitude is never silently converted.
