set term pdf size 6in, 4in font "Helvetica, 32"

in1 = "data"
out1 = "busy-hitmap.pdf"

set output out1

unset key
set view map
# set size ratio -1

pct(v) = (v <= 1.0 ? v * 100.0 : v)
stats in1 matrix using (pct($3)) nooutput
vmin = STATS_min
vmax = STATS_max
if (vmin == vmax) { vmin = vmin - 1.0; vmax = vmax + 1.0 }
black_at = (vmin + 3.0 * vmax) / 4.0
valfmt(v) = (vmax < 10.0 ? sprintf("%.1f%%", v) : sprintf("%.0f%%", v))

# set xlabel "Scheduler" offset 0,0.7
set ylabel "Domain" offset 0,0
set cblabel "Placement (%)" offset -0.8,0

set lmargin 5
set rmargin 3
# set bmargin 3
# set tmargin 1.5

set xrange [-0.5:2.5]
set yrange [3.5:-0.5]
set cbrange [vmin:vmax]

set xtics ("EEVDF" 0, "Chiplet Aff." 1, "Optimal" 2) offset 0,0.4 scale 0
set ytics ("CC0 (25%)" 0, "CC1 (50%)" 1, "CC2 (75%)" 2, "CC3 (100%)" 3) offset 0.2,0 scale 0
set cbtics offset -0.8,0 scale 0
set format cb "%.0f%%"

set border lw 3
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
    in1 matrix using 1:2:(pct($3)) with image pixels, \
    in1 matrix using 1:2:(valfmt(pct($3))):((pct($3) < black_at) ? 0xffffff : 0x111111) with labels center font ",20" tc rgb variable
