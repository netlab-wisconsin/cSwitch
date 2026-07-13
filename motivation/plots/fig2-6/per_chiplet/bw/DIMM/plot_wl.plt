set term pdf size 6in, 4in font "Helvetica, 32"
# set term pdf size 6in, 4in font "Helvetica, 18"

in1 = "data"
out1 = "one-chiplet-per-core-bw-dimm.pdf"

#set xrange [0:32]
#set yrange [0:1.2]

set xlabel "Intra-chiplet Cores (\#)" offset 0,0.7
#set ylabel "10^6 Stall Cycles/Op" offset 0.7,0
set ylabel "Per-core Mem BW. (GB/s)" offset 1,0

set lmargin 7
set rmargin 2

set xtics offset 0,0.4
set ytics offset 0

# set format y "%.0s%c"
#set format y "%.1e"

set grid lw 5

# set key top left vertical samplen 1 box opaque 
set key top right vertical samplen 1 box opaque 
# set key width 1
set key height 0.3

# unset key

set yrange [0:8]
# set ytics 0.5

set xrange [0:]

set output out1

# set format y "%.0f"

# Intel (Red 계열): 1번은 진한 빨강, 3번은 연한 분홍/빨강
# AMD (Green/Blue 계열): 2번은 진한 색, 4번은 연한 색
set style line 1 lc rgb "#6c009b" lw 7 pt 5  # 진한 빨강 (Intel ES)
set style line 3 lc rgb "#d77aff" lw 7 pt 9  # 연한 빨강 (Intel ODB)
set style line 2 lc rgb "#019d72" lw 7 pt 7  # 진한 파랑 (AMD ES)
set style line 4 lc rgb "#5ed8a9" lw 7 pt 11  # 연한 파랑 (AMD ODB)
set style line 5 lc rgb "#d97706" lw 7 pt 13 # 주황 계열
set style line 6 lc rgb "#ea580c" lw 7 pt 1  # 진한 주황 (보조)
set style line 7 lc rgb "#0ea5e9" lw 7 pt 3  # 하늘색 계열


plot \
     in1 using (column("cores")):(column("llama.cpp")/1000) with linespoints ls 1 ps 2 dt 1 title "llama", \
     in1 using (column("cores")):(column("NPB-FT")/1000) with linespoints ls 2  ps 2 dt 1 title "FT", \
     in1 using (column("cores")):(column("NPB-CG")/1000) with linespoints ls 3  ps 2 dt 1 title "CG", \
     in1 using (column("cores")):(column("NPB-MG")/1000) with linespoints ls 4  ps 2 dt 1 title "MG", \
     # in1 using (column("cores")):(column("GAPBS-PageRank")/1000) with linespoints ls 5 ps 2 dt 1 title "PageRank", \
     # in1 using (column("cores")):(column("DuckDB-TPCH-Q21")/1000) with linespoints ls 6  ps 2 dt 1 title "DuckDB", \
     # in1 using (column("cores")):(column("Filebench_fileserver")/1000) with linespoints ls 7  ps 2 dt 1 title "Filebench"
