import java.io.PrintWriter;
import java.nio.file.Files;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.index.LeafReader;
import org.apache.lucene.index.NumericDocValues;
import org.apache.lucene.index.Term;
import org.apache.lucene.search.BooleanClause;
import org.apache.lucene.search.BooleanQuery;
import org.apache.lucene.search.IndexSearcher;
import org.apache.lucene.search.PhraseQuery;
import org.apache.lucene.search.Query;
import org.apache.lucene.search.ScoreDoc;
import org.apache.lucene.search.TermQuery;
import org.apache.lucene.search.TopDocs;
import org.apache.lucene.store.MMapDirectory;

/** Same protocol as lucene-rs/src/bin/bench.rs. */
public class Bench {
  record Q(String kind, String line, Query query) {}

  static List<Q> load(String path) throws Exception {
    List<Q> out = new ArrayList<>();
    for (String l : Files.readAllLines(Paths.get(path))) {
      if (l.isEmpty()) continue;
      String[] kt = l.split("\t");
      String[] terms = kt[1].split(" ");
      Query q;
      if (kt[0].equals("TERM")) {
        q = new TermQuery(new Term("body", terms[0]));
      } else if (kt[0].equals("PHRASE")) {
        PhraseQuery.Builder pb = new PhraseQuery.Builder();
        for (String t : terms) pb.add(new Term("body", t));
        q = pb.build();
      } else {
        BooleanClause.Occur occur =
            kt[0].equals("AND") ? BooleanClause.Occur.MUST : BooleanClause.Occur.SHOULD;
        BooleanQuery.Builder b = new BooleanQuery.Builder();
        for (String t : terms) b.add(new TermQuery(new Term("body", t)), occur);
        q = b.build();
      }
      out.add(new Q(kt[0], l, q));
    }
    return out;
  }

  static double pct(double[] sorted, double p) {
    return sorted[(int) Math.round((sorted.length - 1) * p)];
  }

  public static void main(String[] args) throws Exception {
    long tOpen = System.nanoTime();
    DirectoryReader reader = DirectoryReader.open(new MMapDirectory(Paths.get(args[0])));
    IndexSearcher searcher = new IndexSearcher(reader); // no executor: single-threaded
    searcher.setQueryCache(null);
    double openMs = (System.nanoTime() - tOpen) / 1e6;
    if (reader.leaves().size() != 1) throw new IllegalStateException("expected 1 segment");
    List<Q> queries = load(args[1]);
    String mode = args[2];
    if (mode.equals("dump")) {
      LeafReader leaf = reader.leaves().get(0).reader();
      try (PrintWriter out = new PrintWriter(Files.newBufferedWriter(Paths.get(args[3])))) {
        for (Q q : queries) {
          TopDocs td = searcher.search(q.query, 10);
          StringBuilder sb = new StringBuilder();
          for (ScoreDoc sd : td.scoreDocs) {
            NumericDocValues ids = leaf.getNumericDocValues("id");
            ids.advanceExact(sd.doc);
            if (sb.length() > 0) sb.append(' ');
            sb.append(ids.longValue()).append(':').append(String.format(Locale.ROOT, "%.9e", sd.score));
          }
          out.println(q.line + "\t" + td.totalHits.value() + "\t" + sb);
        }
      }
      return;
    }
    int warmup = Integer.parseInt(args[3]);
    int iters = Integer.parseInt(args[4]);
    long sink = 0;
    long t = System.nanoTime();
    for (Q q : queries) sink += searcher.search(q.query, 10).totalHits.value();
    double coldPassMs = (System.nanoTime() - t) / 1e6;
    for (int w = 0; w < warmup; w++)
      for (Q q : queries) sink += searcher.search(q.query, 10).totalHits.value();
    double[][] times = new double[queries.size()][iters];
    for (int it = 0; it < iters; it++) {
      for (int i = 0; i < queries.size(); i++) {
        long s = System.nanoTime();
        sink += searcher.search(queries.get(i).query, 10).totalHits.value();
        times[i][it] = (System.nanoTime() - s) / 1e3;
      }
    }
    double[] medians = new double[queries.size()];
    for (int i = 0; i < medians.length; i++) {
      Arrays.sort(times[i]);
      medians[i] = times[i][iters / 2];
    }
    StringBuilder sb = new StringBuilder();
    sb.append(String.format(Locale.ROOT, "{\"engine\":\"lucene\",\"open_ms\":%.2f,\"cold_pass_ms\":%.2f", openMs, coldPassMs));
    for (String kind : new String[] {"TERM", "AND", "OR", "PHRASE"}) {
      List<Double> m = new ArrayList<>();
      for (int i = 0; i < medians.length; i++) if (queries.get(i).kind.equals(kind)) m.add(medians[i]);
      double[] s = m.stream().mapToDouble(Double::doubleValue).sorted().toArray();
      if (s.length == 0) continue;
      double mean = Arrays.stream(s).sum() / s.length;
      sb.append(String.format(Locale.ROOT,
          ",\"%s\":{\"n\":%d,\"mean_us\":%.2f,\"p50_us\":%.2f,\"p90_us\":%.2f,\"p99_us\":%.2f}",
          kind, s.length, mean, pct(s, 0.5), pct(s, 0.9), pct(s, 0.99)));
    }
    sb.append(",\"per_query_us\":[");
    for (int i = 0; i < medians.length; i++) sb.append(i == 0 ? "" : ",").append(String.format(Locale.ROOT, "%.2f", medians[i]));
    sb.append("],\"sink\":").append(sink).append('}');
    System.out.println(sb);
  }
}
