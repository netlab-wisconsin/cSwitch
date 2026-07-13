set term pdf size 6in, 4in font "Helvetica, 32"

in1 = "data"
out1 = "bw-ineq-intra-io.pdf"

#set xrange [0:32]
#set yrange [0:1.2]

set xlabel "Traffic Load (\%)" offset 0,0.7
#set ylabel "10^6 Stall Cycles/Op" offset 0.7,0
set ylabel "Norm. Throughput" offset 1.2,0
set y2label "Norm. Latency" offset -2,0

set lmargin 5.5
set rmargin 4.5

set xtics offset 0,0.4
set ytics offset 0.5
set y2tics offset -1.3


# set format y "%.0s%c"
#set format y "%.1e"

set grid lw 5

set key top center horizontal samplen 0.5 box opaque
set key width -0.5
set key height 0.3
# unset key

set yrange [0:1.6]
set y2range [0:3.2]
set ytics 0.5
set y2tics 1

set xrange [0:]

set output out1

set format y "%.1f"

set style line 1 lc rgb "#6c009b" lw 7 pt 5
set style line 3 lc rgb "#d77aff" lw 7 pt 9
set style line 2 lc rgb "#019d72" lw 7 pt 7
set style line 4 lc rgb "#5ed8a9" lw 7 pt 11

set style data histograms
set style histogram clustered gap 1
set style fill pattern border -1

plot \
    in1 using 2:xtic(1) title "DuckDB th." with histograms fill pattern 1, \
     ''  using 3 title "llama th."    with histograms fill pattern 2, \
    in1 using ($0):4 axis x1y2 with linespoints ls 1 ps 2 dt 1 title "DuckDB lat", \
    in1 using ($0):5 axis x1y2 with linespoints ls 2 ps 2 dt 1 title "llama lat."
