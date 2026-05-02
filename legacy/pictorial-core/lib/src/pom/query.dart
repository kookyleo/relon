import '../../pictorial_core.dart';

// "shortcut methods" for the Pictorial Object Model
class Pom {
  History history;

  Pom({History? history}) : history = history ?? History();

  get state => history.state;
}
