set term pdf size 6in, 4in font "Helvetica, 32"

in1 = "rawdata-wl"
out1 = "core-Stall-rocksdb.pdf"

#set xrange [0:32]
#set yrange [0:1.2]

set xlabel "IO Chiplet Load (%)" offset 0,0.7
#set ylabel "10^6 Stall Cycles/Op" offset 0.7,0
set ylabel "Stall Degree" offset -1,0

set lmargin 7
set rmargin 2

set xtics offset 0,0.4
set ytics offset -0.5

# set format y "%.0s%c"
#set format y "%.1e"

set grid lw 5

set key top left horizontal samplen 1 box opaque 
set key width -2
set key height 0.3

set yrange [0:3]
set ytics 1

set xrange [0:]

set output out1

set format y "%.0fM"

# Intel (Red 계열): 1번은 진한 빨강, 3번은 연한 분홍/빨강
# AMD (Green/Blue 계열): 2번은 진한 색, 4번은 연한 색
set style line 1 lc rgb "#6c009b" lw 7 pt 5  # 진한 빨강 (Intel ES)
set style line 3 lc rgb "#d77aff" lw 7 pt 9  # 연한 빨강 (Intel ODB)
set style line 2 lc rgb "#019d72" lw 7 pt 7  # 진한 파랑 (AMD ES)
set style line 4 lc rgb "#5ed8a9" lw 7 pt 11  # 연한 파랑 (AMD ODB)

plot in1 using 1:(column("stall/ops_intel_es")/1000000) with linespoints ls 1 ps 2 dt 1 title "ES (Intel)", \
     in1 using 1:(column("stall/ops_amd_es")/1000000) with linespoints ls 2  ps 2 dt 1 title "ES (AMD)", \
     in1 using 1:(column("stall/ops_intel_orientdb")/1000000) with linespoints ls 3  ps 2 dt 1 title "ODB (Intel)", \
     in1 using 1:(column("stall/ops_amd_orientdb")/1000000) with linespoints ls 4  ps 2 dt 1 title "ODB (AMD)", \
