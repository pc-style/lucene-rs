import java.io.BufferedReader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Paths;
import org.apache.lucene.analysis.Analyzer;
import org.apache.lucene.analysis.Tokenizer;
import org.apache.lucene.analysis.core.WhitespaceTokenizer;
import org.apache.lucene.document.Document;
import org.apache.lucene.document.Field;
import org.apache.lucene.document.FieldType;
import org.apache.lucene.document.NumericDocValuesField;
import org.apache.lucene.index.IndexOptions;
import org.apache.lucene.index.IndexWriter;
import org.apache.lucene.index.IndexWriterConfig;
import org.apache.lucene.index.LogByteSizeMergePolicy;
import org.apache.lucene.store.FSDirectory;

/** Indexes the normalized corpus (one doc per line) into a single-segment Lucene index. */
public class Indexer {
  public static void main(String[] args) throws Exception {
    Analyzer analyzer =
        new Analyzer() {
          @Override
          protected TokenStreamComponents createComponents(String fieldName) {
            Tokenizer t = new WhitespaceTokenizer();
            return new TokenStreamComponents(t);
          }
        };
    FieldType ft = new FieldType();
    ft.setTokenized(true);
    boolean positions = args.length > 2 && args[2].equals("--positions");
    ft.setIndexOptions(positions ? IndexOptions.DOCS_AND_FREQS_AND_POSITIONS : IndexOptions.DOCS_AND_FREQS);
    ft.freeze();

    IndexWriterConfig iwc = new IndexWriterConfig(analyzer);
    iwc.setOpenMode(IndexWriterConfig.OpenMode.CREATE);
    iwc.setRAMBufferSizeMB(2000);
    iwc.setUseCompoundFile(false);
    // Log merge policy merges adjacent segments only, so doc IDs keep corpus order.
    iwc.setMergePolicy(new LogByteSizeMergePolicy());
    long t0 = System.nanoTime();
    int n = 0;
    try (FSDirectory dir = FSDirectory.open(Paths.get(args[1]));
        IndexWriter w = new IndexWriter(dir, iwc);
        BufferedReader r = Files.newBufferedReader(Paths.get(args[0]), StandardCharsets.UTF_8)) {
      Document doc = new Document();
      Field body = new Field("body", "", ft);
      NumericDocValuesField id = new NumericDocValuesField("id", 0);
      doc.add(body);
      doc.add(id);
      for (String line; (line = r.readLine()) != null; ) {
        body.setStringValue(line);
        id.setLongValue(n++);
        w.addDocument(doc);
      }
      double invert = (System.nanoTime() - t0) / 1e9;
      w.forceMerge(1);
      w.commit();
      System.out.printf(
          "{\"engine\":\"lucene\",\"docs\":%d,\"index_secs\":%.2f,\"total_secs_incl_forcemerge\":%.2f}%n",
          n, invert, (System.nanoTime() - t0) / 1e9);
    }
  }
}
