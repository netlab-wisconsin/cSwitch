set term pdf size 6in, 5in font "Helvetica, 32"

perf_in = "../free-perf/data"
heat_in = "../free-hitmap/data"
out1 = "free-merged.pdf"

pct(v) = (v <= 1.0 ? v * 100.0 : v)
stats heat_in matrix using (pct($3)) nooutput
vmin = STATS_min
vmax = STATS_max
if (vmin == vmax) { vmin = vmin - 1.0; vmax = vmax + 1.0 }
black_at = (vmin + 3.0 * vmax) / 4.0
valfmt(v) = (vmax < 10.0 ? sprintf("%.1f%%", v) : sprintf("%.0f%%", v))

set output out1
set multiplot

unset key
unset colorbox
unset arrow

set lmargin at screen 0.23
set rmargin at screen 0.93
set bmargin at screen 0.7
set tmargin at screen 0.96

set ylabel "Time (s)" offset -0,0
unset xlabel

set xrange [-0.5:2.5]
set yrange [0:0.3]
# set xtics ("EEVDF" 0, "Optimal" 1, "Chiplet Aff." 2) offset 0,0.2 scale 0
unset xtics
set ytics 0.1 offset 0.2,0

set border lw 3
set grid ytics lw 4
set boxwidth 0.35 absolute
set style fill solid 1.0 border -1

plot \
    perf_in every ::0::0 using (0):1 with boxes fillstyle pattern 1 notitle, \
    ''      every ::1::1 using (1):1 with boxes fillstyle pattern 2 notitle, \
    ''      every ::2::2 using (2):1 with boxes fillstyle pattern 4 notitle

unset grid
set colorbox
unset arrow

set lmargin at screen 0.23
set rmargin at screen 0.93
set bmargin at screen 0.12
set tmargin at screen 0.66

set ylabel "Compute Chiplet ID" offset 3.1,0
# set cblabel "Placement (%)" offset -0.8,0

set xrange [-0.5:2.5]
set yrange [3.5:-0.5]
set cbrange [vmin:vmax]

set xtics ("EEVDF" 0, "Chiplet-Aff." 1, "Oracle" 2) offset 0,0.2 scale 0
set ytics ("0(1/4)" 0, "1(1/2)" 1, "2(3/4)" 2, "3(Full)" 3) offset 0.6,0 scale 0
# set ytics ("CC0" 0, "CC1" 1, "CC2" 2, "CC3" 3) offset 0.2,0 scale 0
# set cbtics offset -0.8,0 scale 0
unset cbtics
# set format cb "%.0f%%"

set palette defined ( \
    0 "#440154", \
    1 "#482878", \
    2 "#3e4989", \
    3 "#31688e", \
    4 "#26828e", \
    5 "#1f9e89", \
    6 "#35b779", \
    7 "#6ece58", \
    8 "#b5de2b", \
    9 "#fde725" \
)

set arrow 1 from -0.5,-0.5 to  2.5,-0.5 nohead front lw 2 lc rgb "#303030"
set arrow 2 from -0.5, 0.5 to  2.5, 0.5 nohead front lw 2 lc rgb "#303030"
set arrow 3 from -0.5, 1.5 to  2.5, 1.5 nohead front lw 2 lc rgb "#303030"
set arrow 4 from -0.5, 2.5 to  2.5, 2.5 nohead front lw 2 lc rgb "#303030"
set arrow 5 from -0.5, 3.5 to  2.5, 3.5 nohead front lw 2 lc rgb "#303030"
set arrow 6 from -0.5,-0.5 to -0.5, 3.5 nohead front lw 2 lc rgb "#303030"
set arrow 7 from  0.5,-0.5 to  0.5, 3.5 nohead front lw 2 lc rgb "#303030"
set arrow 8 from  1.5,-0.5 to  1.5, 3.5 nohead front lw 2 lc rgb "#303030"
set arrow 9 from  2.5,-0.5 to  2.5, 3.5 nohead front lw 2 lc rgb "#303030"

plot \
    heat_in matrix using 1:2:(pct($3)) with image pixels, \
    heat_in matrix using 1:2:(valfmt(pct($3))):((pct($3) < black_at) ? 0xffffff : 0x111111) with labels center font ",32" tc rgb variable

unset multiplot
