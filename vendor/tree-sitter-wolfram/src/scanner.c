#include "tree_sitter/parser.h"

#include <wctype.h>

enum TokenType {
  COMMENT
};

static bool scan_comment(TSLexer *lexer) {
  int depth = 1;

  while (!lexer->eof(lexer)) {
    switch (lexer->lookahead) {
      case '*':
        lexer->advance(lexer, false);
        if (lexer->lookahead == ')') {
          depth--;
          if (depth == 0) {
            lexer->result_symbol = COMMENT;
            lexer->advance(lexer, false);
            lexer->mark_end(lexer);
            return true;
          }
        }
        break;

      case '(':
        lexer->advance(lexer, false);
        if (lexer->lookahead == '*') {
          depth++;
        }
        break;

      default:
        lexer->advance(lexer, false);
        break;
    }
  }

  return false;
}

void *tree_sitter_wolfram_external_scanner_create(void) {
  return NULL;
}

void tree_sitter_wolfram_external_scanner_destroy(void *payload) {
}

unsigned tree_sitter_wolfram_external_scanner_serialize(void *payload, char *buffer) {
  return 0;
}

void tree_sitter_wolfram_external_scanner_deserialize(void *payload, const char *buffer, unsigned length) {
}

bool tree_sitter_wolfram_external_scanner_scan(void *payload, TSLexer *lexer, const bool *valid_symbols) {
  while (iswspace(lexer->lookahead)) {
    lexer->advance(lexer, true);
  }

  if (lexer->lookahead == '(') {
    lexer->advance(lexer, false);
    if (lexer->lookahead == '*') {
      lexer->advance(lexer, false);
      return scan_comment(lexer);
    }
  }

  return false;
}
