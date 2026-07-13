set term pdf size 6in, 2in font "Helvetica, 32"

in1 = "data"
out1 = "free-perf.pdf"

stats in1 every ::0::2 using 1 nooutput
ymax = (STATS_records > 0 && STATS_max > 0 ? STATS_max * 1.15 : 1)

set output out1

unset key

# set xlabel "Scheduler" offset 0,0.5
set ylabel "Time (s)" offset 2,0

set lmargin 6
set rmargin 2
# set bmargin 3
set tmargin 1

set xtics ("EEVDF" 0, "Optimal" 1, "Chiplet Aff." 2) offset 0,0.2 scale 0
set ytics 0.1 offset 0.2,0

set xrange [-0.5:2.5]
set yrange [0:0.4]

set border lw 3
set grid ytics lw 4
set boxwidth 0.55 absolute
set style fill solid 1.0 border -1

plot \
    in1 every ::0::0 using (0):1 with boxes lc rgb "#6c009b" notitle, \
    ''  every ::1::1 using (1):1 with boxes lc rgb "#019d72" notitle, \
    ''  every ::2::2 using (2):1 with boxes lc rgb "#d97706" notitle
